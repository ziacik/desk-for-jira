use std::{collections::{HashMap, HashSet}, fmt, io::Read, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PAGE_SIZE: usize = 100;
const MAX_ITEMS: usize = 500;
const FIELDS: [&str; 13] = [
    "summary",
    "status",
    "assignee",
    "issuetype",
    "priority",
    "fixVersions",
    "versions",
    "updated",
    "description",
    "attachment",
    "reporter",
    "project",
    "parent",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Cloud,
    Server,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub base_url: String,
    pub mode: AuthMode,
    pub email: String,
    pub token: String,
}

impl Credentials {
    pub fn validate(&mut self) -> Result<()> {
        self.base_url = self.base_url.trim().trim_end_matches('/').to_owned();
        self.email = self.email.trim().to_owned();
        self.token = self.token.trim().to_owned();
        let url = reqwest::Url::parse(&self.base_url).context("Enter a valid Jira URL")?;
        ensure!(url.scheme() == "https", "Jira URL must use HTTPS");
        ensure!(url.host_str().is_some(), "Jira URL must have a hostname");
        ensure!(url.username().is_empty() && url.password().is_none(), "Do not include credentials in the URL");
        ensure!(url.query().is_none() && url.fragment().is_none(), "Enter the Jira base URL without query or fragment");
        ensure!(!self.token.is_empty(), "Enter an API or personal access token");
        if self.mode == AuthMode::Cloud {
            ensure!(!self.email.is_empty(), "Enter your Atlassian account email");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Epic {
    pub key: String,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueParent {
    pub key: String,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub key: String,
    pub project_key: String,
    pub summary: String,
    pub status: String,
    pub status_id: String,
    pub status_category: Option<StatusCategory>,
    pub kind: String,
    pub priority: String,
    pub fix_versions: Vec<String>,
    pub affects_versions: Vec<String>,
    pub assignee: String,
    pub updated: String,
    pub description: String,
    pub description_doc: Option<Value>,
    pub attachments: Vec<Attachment>,
    pub is_epic: bool,
    pub epic: Option<Epic>,
    pub parent: Option<IssueParent>,
}

#[derive(Clone, Debug)]
pub struct IssueComment {
    pub author: String,
    pub created: String,
    pub body: Value,
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub content_url: String,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        self.mime_type.to_ascii_lowercase().starts_with("image/")
    }
}


/// A rejected Jira transition with the original HTTP status and structured
/// field keys. Human-readable error messages must never be reverse-parsed to
/// determine field identity.
#[derive(Debug)]
pub struct JiraTransitionError {
    pub status: u16,
    pub message: String,
    pub field_keys: Vec<String>,
}

impl JiraTransitionError {
    fn from_response(status: u16, body: &str) -> Self {
        let field_keys = serde_json::from_str::<Value>(body).ok()
            .and_then(|response| response.get("errors").and_then(Value::as_object).cloned())
            .map(|errors| errors.into_iter().map(|(key, _)| key).collect())
            .unwrap_or_default();
        Self {
            status,
            message: jira_error_message(body),
            field_keys,
        }
    }

    pub fn is_validation_failure(&self) -> bool {
        matches!(self.status, 400 | 422)
    }
}

impl fmt::Display for JiraTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Jira HTTP {}: {}", self.status, self.message)
    }
}

impl std::error::Error for JiraTransitionError {}

#[derive(Clone, Debug)]
pub struct TransitionChoice {
    pub label: String,
    pub value: Value,
}

#[derive(Clone, Debug)]
pub struct TransitionField {
    pub id: String,
    pub name: String,
    pub schema_type: String,
    pub item_type: String,
    pub custom_type: String,
    pub required: bool,
    pub choices: Vec<TransitionChoice>,
}

impl TransitionField {
    pub fn is_multiline(&self) -> bool {
        matches!(self.id.as_str(), "description" | "environment" | "comment")
            || self.custom_type.ends_with(":textarea")
    }

    pub fn is_worklog(&self) -> bool {
        self.id == "worklog" || self.schema_type == "worklog"
    }

    pub fn uses_structured_input(&self) -> bool {
        self.choices.is_empty()
            && !matches!(
                self.schema_type.as_str(),
                "string" | "number" | "date" | "datetime" | "boolean" | "user"
                    | "option" | "resolution" | "priority" | "version"
                    | "component" | "issuetype" | "project"
            )
            && !(self.schema_type == "array"
                && matches!(
                    self.item_type.as_str(),
                    "string" | "option" | "version" | "component"
                ))
    }

    pub fn text_value(&self, raw: &str, cloud: bool) -> Result<Value> {
        let value = raw.trim();
        ensure!(!value.is_empty(), "{} is required", self.name);
        if self.id == "comment" {
            return if cloud {
                Ok(json!({
                    "type": "doc",
                    "version": 1,
                    "content": [{"type": "paragraph", "content": [{"type": "text", "text": value}]}]
                }))
            } else {
                Ok(json!(value))
            };
        }
        match self.schema_type.as_str() {
            "number" => {
                let number = value.parse::<f64>()
                    .with_context(|| format!("{} must be a number", self.name))?;
                ensure!(number.is_finite(), "{} must be a finite number", self.name);
                Ok(json!(number))
            }
            "date" => {
                let bytes = value.as_bytes();
                ensure!(
                    bytes.len() == 10 && bytes[4] == b'-' && bytes[7] == b'-'
                        && bytes.iter().enumerate().all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit()),
                    "{} must use YYYY-MM-DD", self.name
                );
                Ok(json!(value))
            }
            "datetime" => Ok(json!(value)),
            "boolean" => match value.to_ascii_lowercase().as_str() {
                "true" => Ok(json!(true)),
                "false" => Ok(json!(false)),
                _ => bail!("{} must be true or false", self.name),
            },
            "string" if cloud && self.is_multiline() => Ok(json!({
                "type": "doc",
                "version": 1,
                "content": [{"type": "paragraph", "content": [{"type": "text", "text": value}]}]
            })),
            "string" => Ok(json!(value)),
            "user" if cloud => Ok(json!({"accountId": value})),
            "user" => Ok(json!({"name": value})),
            "option" | "resolution" | "priority" | "version" | "component"
            | "issuetype" | "project" => Ok(json!({"id": value})),
            "array" if self.item_type == "string" => {
                let items: Vec<_> = value.split(',').map(str::trim)
                    .filter(|part| !part.is_empty()).collect();
                ensure!(!items.is_empty(), "{} is required", self.name);
                Ok(json!(items))
            }
            "array" if matches!(self.item_type.as_str(), "option" | "version" | "component") => {
                let items: Vec<_> = value.split(',').map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(|id| json!({"id": id})).collect();
                ensure!(!items.is_empty(), "{} is required", self.name);
                Ok(json!(items))
            }
            _ => {
                let parsed: Value = serde_json::from_str(value)
                    .with_context(|| format!("{} requires a valid Jira JSON value", self.name))?;
                ensure!(!parsed.is_null(), "{} is required", self.name);
                Ok(parsed)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct IssueTransition {
    pub id: String,
    pub name: String,
    pub to_name: String,
    pub to_id: String,
    pub to_category: Option<StatusCategory>,
    /// Jira Cloud exposes whether a transition is global; absent on older servers.
    pub is_global: Option<bool>,
    /// Explicit Jira Data Center opsbar ordering, when returned by the API.
    pub opsbar_sequence: Option<i64>,
    pub fields: Vec<TransitionField>,
    // Fields editable on the issue but absent from this transition screen.
    pub edit_fields: Vec<TransitionField>,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub key: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusCategory {
    Todo,
    InProgress,
    Done,
}

#[derive(Clone, Debug)]
pub struct ProjectStatus {
    pub project_key: String,
    pub id: String,
    pub name: String,
    pub category: Option<StatusCategory>,
    pub workflow_order: Option<usize>,
}

fn parse_status_category(value: &Value) -> Option<StatusCategory> {
    let key = value.as_str().or_else(|| value.get("key").and_then(Value::as_str))?;
    match key.to_ascii_lowercase().as_str() {
        "new" | "todo" | "to_do" | "to do" => Some(StatusCategory::Todo),
        "indeterminate" | "in_progress" | "in progress" | "in-flight" => Some(StatusCategory::InProgress),
        "done" | "completed" => Some(StatusCategory::Done),
        _ => None,
    }
}


#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub title: String,
    pub url: String,
    pub status: String,
}

/// The private Jira development endpoint can return PRs alongside branches,
/// and its singular/plural "detail" response keys vary by Jira version.
fn parse_dev_pull_requests(payload: &Value) -> Vec<PullRequest> {
    let details = payload.get("detail").or_else(|| payload.get("details"));
    let entries: Vec<&Value> = match details {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(item) if item.is_object() => vec![item],
        _ => Vec::new(),
    };

    let mut requests = Vec::new();
    for entry in entries {
        for field in ["pullRequests", "pullrequests"] {
            let Some(items) = entry.get(field).and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                let url = item.get("url")
                    .or_else(|| item.get("link"))
                    .or_else(|| item.get("displayUrl"))
                    .and_then(Value::as_str);
                let Some(url) = url.filter(|url| reqwest::Url::parse(url).is_ok_and(
                    |parsed| matches!(parsed.scheme(), "https" | "http")
                )) else {
                    continue;
                };
                let title = item.get("name")
                    .or_else(|| item.get("title"))
                    .or_else(|| item.get("displayName"))
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or("Pull request");
                let status = item.get("status")
                    .or_else(|| item.get("state"))
                    .or_else(|| item.get("pullRequestStatus"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                requests.push(PullRequest {
                    title: title.to_owned(),
                    url: url.to_owned(),
                    status: status.to_ascii_uppercase(),
                });
            }
        }
    }
    requests
}

pub struct JiraClient {
    account: Credentials,
    http: Client,
}

impl JiraClient {
    pub fn new(mut account: Credentials) -> Result<Self> {
        account.validate()?;
        Ok(Self {
            account,
            http: Client::builder()
                .timeout(Duration::from_secs(25))
                .user_agent("DeskForJira/0.1")
                .build()?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.account.base_url, path)
    }

    fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match self.account.mode {
            AuthMode::Cloud => request.basic_auth(&self.account.email, Some(&self.account.token)),
            AuthMode::Server => request.bearer_auth(&self.account.token),
        }
    }

    fn read_json(&self, request: RequestBuilder) -> Result<Value> {
        let response = self.authorize(request)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .context("Could not connect to Jira")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|json| json.get("errorMessages").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("Request failed").to_owned());
            bail!("Jira HTTP {}: {}", status.as_u16(), message);
        }
        response.json().context("Jira returned invalid JSON")
    }



    /// Read PR URLs reported by Jira's Development panel. Jira does not expose
    /// a supported read API for this information. The private dev-status API
    /// works with the same basic email/token login as the Cloud REST client,
    /// but can change without warning.
    pub fn issue_pull_requests(&self, key: &str) -> Result<Vec<PullRequest>> {
        let Some((project, number)) = key.split_once('-') else {
            bail!("Invalid Jira issue key");
        };
        ensure!(
            !project.is_empty()
                && project.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && !number.is_empty()
                && number.bytes().all(|byte| byte.is_ascii_digit()),
            "Invalid Jira issue key"
        );

        // dev-status requires the numeric issue ID, not the human-readable key.
        let version = if self.account.mode == AuthMode::Cloud { "3" } else { "2" };
        let issue = self.read_json(self.http.get(
            self.url(&format!("/rest/api/{version}/issue/{key}"))
        ).query(&[("fields", "summary")]))?;
        let id = issue.get("id").and_then(Value::as_str)
            .context("Jira did not return a numeric issue ID")?;
        // Newer Jira Cloud instances expose the complete development detail
        // without applicationType or dataType. Prefer this single request so
        // third-party integration IDs need not be guessed.
        if let Ok(payload) = self.read_json(self.http.get(
            self.url("/rest/dev-status/latest/issue/details")
        ).query(&[("issueId", id)])) {
            let requests = parse_dev_pull_requests(&payload);
            if !requests.is_empty() {
                let mut unique = HashSet::new();
                return Ok(requests.into_iter()
                    .filter(|request| unique.insert(request.url.clone()))
                    .collect());
            }
        }
        let summary = self.read_json(self.http.get(
            self.url("/rest/dev-status/latest/issue/summary")
        ).query(&[("issueId", id)]))?;

        let expected_count = summary.pointer("/summary/pullrequest/overall/count")
            .and_then(Value::as_u64).unwrap_or(0);
        let mut providers = HashSet::new();
        // Some integrations put PRs inside the branch detail response.
        for kind in ["pullrequest", "branch"] {
            if let Some(instances) = summary.pointer(&format!("/summary/{kind}/byInstanceType"))
                .and_then(Value::as_object)
            {
                for (name, info) in instances {
                    if info.get("count").and_then(Value::as_u64) != Some(0) {
                        providers.insert(name.clone());
                    }
                }
            }
        }
        if providers.is_empty() {
            if expected_count == 0 {
                return Ok(Vec::new());
            }
            bail!("Jira reports pull requests but no development provider");
        }

        let mut providers: Vec<_> = providers.into_iter().collect();
        providers.sort();
        let mut requests = Vec::new();
        let mut seen = HashSet::new();
        for provider in providers {
            for kind in ["pullrequest", "branch"] {
                // In 2026 Atlassian also began serving some installations via
                // /details (plural), while others still require /detail.
                for endpoint in ["details", "detail"] {
                    let path = format!("/rest/dev-status/latest/issue/{endpoint}");
                    let result = self.read_json(self.http.get(self.url(&path)).query(&[
                        ("issueId", id),
                        ("applicationType", provider.as_str()),
                        ("dataType", kind),
                    ]));
                    let Ok(payload) = result else {
                        continue;
                    };
                    let found = parse_dev_pull_requests(&payload);
                    let has_prs = !found.is_empty();
                    for request in found {
                        if seen.insert(request.url.clone()) {
                            requests.push(request);
                        }
                    }
                    if has_prs {
                        break;
                    }
                }
            }
        }
        if requests.is_empty() && expected_count > 0 {
            bail!("Jira reports {expected_count} pull request(s), but the development API returned no PR links");
        }
        Ok(requests)
    }

    pub fn issue_transitions(&self, key: &str) -> Result<Vec<IssueTransition>> {
        let version = if self.account.mode == AuthMode::Cloud { "3" } else { "2" };
        let path = format!("/rest/api/{version}/issue/{key}/transitions");
        let request = self.http.get(self.url(&path));
        let request = if self.account.mode == AuthMode::Cloud {
            // Cloud can order transitions by Jira's configured OpsBar priority.
            request.query(&[
                ("expand", "transitions.fields"),
                ("sortByOpsBarAndStatus", "true"),
            ])
        } else {
            // Data Center exposes opsbarSequence in each transition instead.
            request.query(&[("expand", "transitions.fields")])
        };
        let data = self.read_json(request)?;
        let mut transitions = parse_issue_transitions(&data);
        if self.account.mode != AuthMode::Cloud {
            // Stable: transitions with equal priority keep the API's ordering.
            transitions.sort_by_key(|transition| transition.opsbar_sequence.unwrap_or(i64::MAX));
        }

        // An issue field may already be populated even when its transition-screen
        // metadata says "required". Never ask the user to re-enter that value.
        // Query the current issue after collecting both screen and edit metadata.
        let edit_path = format!("/rest/api/{version}/issue/{key}/editmeta");
        if let Ok(editmeta) = self.read_json(self.http.get(self.url(&edit_path)))
            && let Some(meta_fields) = editmeta.get("fields").and_then(Value::as_object)
        {
            let editable = parse_transition_fields(meta_fields);
            for transition in &mut transitions {
                transition.edit_fields = editable.iter().filter(|field| {
                    field.id != "comment" && !transition.fields.iter()
                        .any(|screen_field| screen_field.id == field.id)
                }).cloned().collect();
            }
        }
        let field_ids: HashSet<String> = transitions.iter()
            .flat_map(|transition| transition.fields.iter().chain(&transition.edit_fields))
            .filter(|field| !field.is_worklog())
            .map(|field| field.id.clone())
            .collect();
        if !field_ids.is_empty() {
            let names = field_ids.into_iter().collect::<Vec<_>>().join(",");
            let issue_path = format!("/rest/api/{version}/issue/{key}");
            // If the current issue values cannot be read, fail explicitly.
            // Guessing that a field is empty would prompt for values that
            // may already be set and could overwrite valid issue data.
            let issue = self.read_json(self.http.get(self.url(&issue_path))
                .query(&[("fields", names)]))
                .with_context(|| format!("Could not read current fields for {key}"))?;
            if let Some(values) = issue.get("fields").and_then(Value::as_object) {
                for transition in &mut transitions {
                    remove_populated_fields(transition, values);
                }
            }
        }
        Ok(transitions)
    }

    pub fn edit_issue_fields(&self, key: &str, fields: serde_json::Map<String, Value>) -> Result<()> {
        if fields.is_empty() {
            return Ok(());
        }
        let version = if self.account.mode == AuthMode::Cloud { "3" } else { "2" };
        let path = format!("/rest/api/{version}/issue/{key}");
        let response = self.authorize(self.http.put(self.url(&path))
            .json(&json!({"fields": fields})))
            .header(reqwest::header::ACCEPT, "application/json")
            .send().context("Could not connect to Jira")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("Jira HTTP {}: {}", status.as_u16(), jira_error_message(&body));
        }
        Ok(())
    }

    pub fn add_worklog(&self, key: &str, worklog: Value) -> Result<()> {
        let version = if self.account.mode == AuthMode::Cloud { "3" } else { "2" };
        let path = format!("/rest/api/{version}/issue/{key}/worklog");
        let response = self.authorize(self.http.post(self.url(&path))
            .json(&worklog))
            .header(reqwest::header::ACCEPT, "application/json")
            .send().context("Could not connect to Jira")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("Jira HTTP {}: {}", status.as_u16(), jira_error_message(&body));
        }
        Ok(())
    }

    pub fn transition_issue(&self, key: &str, transition_id: &str, fields: serde_json::Map<String, Value>) -> Result<()> {
        let version = if self.account.mode == AuthMode::Cloud { "3" } else { "2" };
        let path = format!("/rest/api/{version}/issue/{key}/transitions");
        let payload = transition_payload(transition_id, fields);
        let response = self.authorize(self.http.post(self.url(&path)).json(&payload))
            .header(reqwest::header::ACCEPT, "application/json")
            .send().context("Could not connect to Jira")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(JiraTransitionError::from_response(status.as_u16(), &body).into());
        }
        Ok(())
    }

    pub fn myself(&self) -> Result<String> {
        let version = if self.account.mode == AuthMode::Cloud { "3" } else { "2" };
        let data = self.read_json(self.http.get(self.url(&format!("/rest/api/{version}/myself"))))?;
        Ok(data.get("displayName").and_then(Value::as_str)
            .or_else(|| data.get("name").and_then(Value::as_str))
            .unwrap_or("Jira user").to_owned())
    }

    pub fn search_issues(&self, jql: &str) -> Result<Vec<Issue>> {
        self.search_issues_with_epic_link(jql, None)
    }

    /// A single page is enough for the command palette. Avoid fetching the
    /// hundreds of results used for full project-board loading.
    pub fn quick_search_issues(&self, jql: &str) -> Result<Vec<Issue>> {
        let data = match self.account.mode {
            AuthMode::Cloud => self.read_json(
                self.http.post(self.url("/rest/api/3/search/jql"))
                    .json(&json!({"jql": jql, "maxResults": 30, "fields": FIELDS}))
            )?,
            AuthMode::Server => self.read_json(
                self.http.post(self.url("/rest/api/2/search"))
                    .json(&json!({"jql": jql, "maxResults": 30, "fields": FIELDS}))
            )?,
        };
        Ok(parse_issues(&data))
    }

    /// Fetch a single issue by its exact key, independently of board filters and
    /// search indexing. A 404 is returned as an error, not an empty candidate.
    pub fn issue_by_key(&self, key: &str) -> Result<Issue> {
        let Some((project, number)) = key.split_once('-') else {
            bail!("Invalid Jira issue key");
        };
        ensure!(
            !project.is_empty()
                && project.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && !number.is_empty()
                && number.bytes().all(|byte| byte.is_ascii_digit()),
            "Invalid Jira issue key"
        );
        let version = if self.account.mode == AuthMode::Cloud { "3" } else { "2" };
        let path = format!("/rest/api/{version}/issue/{key}");
        let data = self.read_json(
            self.http.get(self.url(&path))
                .query(&[("fields", FIELDS.join(","))])
        )?;
        parse_issue(&data, None).context("Jira returned an invalid issue response")
    }

    /// Fetch only current workflow status names for the worktree list.
    /// Unlike full issue retrieval, this avoids descriptions, attachments,
    /// epic enrichment, and any assignee/board filtering.
    pub fn issue_statuses_by_keys(&self, keys: &[String]) -> Result<HashMap<String, String>> {
        let mut statuses = HashMap::new();
        for batch in keys.chunks(50) {
            if batch.is_empty() { continue; }
            // Reuse the validated key-IN query to avoid embedding arbitrary
            // branch text into JQL.
            let jql = pinned_issues_jql(batch)?;
            let data = match self.account.mode {
                AuthMode::Cloud => self.read_json(
                    self.http.post(self.url("/rest/api/3/search/jql")).json(
                        &json!({"jql": jql, "maxResults": 50, "fields": ["status"]}),
                    ),
                )?,
                AuthMode::Server => self.read_json(
                    self.http.post(self.url("/rest/api/2/search")).json(
                        &json!({"jql": jql, "maxResults": 50, "startAt": 0, "fields": ["status"]}),
                    ),
                )?,
            };
            statuses.extend(parse_issue_statuses(&data));
        }
        Ok(statuses)
    }

    /// Direct children are requested independently of board filters and assignee.
    /// Jira's parent JQL works for subtasks and modern issue hierarchies.
    pub fn child_issues(&self, parent_key: &str) -> Result<Vec<Issue>> {
        self.search_issues(&child_issues_jql(parent_key)?)
    }

    fn search_issues_with_epic_link(&self, jql: &str, epic_link_field: Option<&str>) -> Result<Vec<Issue>> {
        let mut fields = FIELDS.to_vec();
        if let Some(field) = epic_link_field {
            fields.push(field);
        }
        let mut result = Vec::new();
        let mut next_token: Option<String> = None;
        let mut start_at = 0usize;

        loop {
            let data = match self.account.mode {
                AuthMode::Cloud => {
                    let mut payload = json!({
                        "jql": jql,
                        "maxResults": PAGE_SIZE,
                        "fields": fields,
                    });
                    if let Some(ref token) = next_token {
                        payload["nextPageToken"] = json!(token);
                    }
                    self.read_json(self.http.post(self.url("/rest/api/3/search/jql")).json(&payload))?
                }
                AuthMode::Server => self.read_json(
                    self.http.post(self.url("/rest/api/2/search"))
                        .json(&json!({"jql":jql, "maxResults":PAGE_SIZE, "startAt":start_at, "fields":fields}))
                )?,
            };
            let page = if epic_link_field.is_some() {
                parse_issues_with_epic_link(&data, epic_link_field)
            } else {
                parse_issues(&data)
            };
            let count = page.len();
            result.extend(page);
            if count == 0 || result.len() >= MAX_ITEMS {
                break;
            }
            if self.account.mode == AuthMode::Cloud {
                next_token = data.get("nextPageToken").and_then(Value::as_str).map(str::to_owned);
                if next_token.is_none() || data.get("isLast").and_then(Value::as_bool) == Some(true) {
                    break;
                }
            } else {
                start_at += count;
                if start_at >= data.get("total").and_then(Value::as_u64).unwrap_or(start_at as u64) as usize {
                    break;
                }
            }
        }
        result.truncate(MAX_ITEMS);
        Ok(result)
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut projects = Vec::new();
        if self.account.mode == AuthMode::Server {
            let data = self.read_json(self.http.get(self.url("/rest/api/2/project")))?;
            let values = data.as_array().context("Unexpected Jira projects response")?;
            projects.extend(values.iter().filter_map(parse_project));
        } else {
            let mut start = 0usize;
            loop {
                let data = self.read_json(self.http.get(self.url("/rest/api/3/project/search"))
                    .query(&[("startAt", start), ("maxResults", PAGE_SIZE)]))?;
                let values = data.get("values").and_then(Value::as_array)
                    .context("Unexpected Jira projects response")?;
                let count = values.len();
                projects.extend(values.iter().filter_map(parse_project));
                start += count;
                if count == 0 || data.get("isLast").and_then(Value::as_bool) == Some(true) {
                    break;
                }
            }
        }
        projects.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(projects)
    }

    pub fn project_statuses(&self, keys: &[String]) -> Result<Vec<ProjectStatus>> {
        let version = if self.account.mode == AuthMode::Cloud { "3" } else { "2" };
        let mut statuses = Vec::new();
        let mut seen = HashSet::new();
        for key in keys {
            validate_project_key(key)?;
            let data = self.read_json(self.http.get(
                self.url(&format!("/rest/api/{version}/project/{key}/statuses"))
            ))?;
            let types = data.as_array().context("Unexpected Jira statuses response")?;
            for issue_type in types {
                for status in issue_type.get("statuses").and_then(Value::as_array)
                    .into_iter().flatten() {
                    let (Some(id), Some(name)) = (
                        status.get("id").and_then(Value::as_str),
                        status.get("name").and_then(Value::as_str),
                    ) else { continue };
                    if seen.insert((key.clone(), id.to_owned())) {
                        statuses.push(ProjectStatus {
                            project_key: key.clone(),
                            id: id.to_owned(),
                            name: name.to_owned(),
                            category: status.get("statusCategory").and_then(parse_status_category),
                            workflow_order: None,
                        });
                    }
                }
            }
        }
        // The project-status endpoint can omit statusCategory. The regular
        // status endpoint provides it to users with Browse Projects permission.
        // This is optional metadata: if it is inaccessible, the board still loads.
        if statuses.iter().any(|status| status.category.is_none())
            && let Ok(data) = self.read_json(self.http.get(
                self.url(&format!("/rest/api/{version}/status"))
            ))
            && let Some(all) = data.as_array()
        {
            let categories: std::collections::HashMap<_, _> = all.iter()
                .filter_map(|status| Some((
                    status.get("id")?.as_str()?,
                    parse_status_category(status.get("statusCategory")?)?,
                )))
                .collect();
            for status in &mut statuses {
                if status.category.is_none() {
                    status.category = categories.get(status.id.as_str()).copied();
                }
            }
        }
        if self.account.mode == AuthMode::Cloud {
            for key in keys {
                let workflow_order = self.project_workflow_order(key);
                for status in statuses.iter_mut().filter(|s| &s.project_key == key) {
                    status.workflow_order = workflow_order.get(status.id.as_str()).copied();
                }
            }
        }
        Ok(statuses)
    }

    // Optional best-effort workflow metadata. Many Jira users have browse access
    // but lack View (read-only) workflow permission; never block the board on it.
    fn project_workflow_order(&self, key: &str) -> std::collections::HashMap<String, usize> {
        let project = self.read_json(self.http.get(
            self.url(&format!("/rest/api/3/project/{key}"))
        ));
        let Some(project_id) = project.ok()
            .and_then(|p| p.get("id").and_then(Value::as_str).map(str::to_owned))
        else { return std::collections::HashMap::new(); };

        let mut result = std::collections::HashMap::new();
        let mut start = 0usize;
        for _ in 0..10 {
            let page = self.read_json(self.http.get(
                self.url("/rest/api/3/workflows/search")
            ).query(&[
                ("projectId", project_id.as_str()),
                ("startAt", &start.to_string()),
                ("maxResults", "100"),
                ("isActive", "true"),
            ]));
            let Ok(page) = page else { break; };
            for (id, position) in parse_workflow_status_order(&page) {
                result.entry(id).and_modify(|old: &mut usize| {
                    *old = (*old).min(position);
                }).or_insert(position);
            }
            let count = page.get("values").and_then(Value::as_array)
                .map_or(0, Vec::len);
            if count == 0 || page.get("isLast").and_then(Value::as_bool) == Some(true) {
                break;
            }
            start += count;
        }
        result
    }

    /// Download a Jira attachment using the stored credentials. Only the same
    /// HTTPS origin may be requested; reqwest drops credentials on CDN redirects.
    pub fn image_bytes(&self, attachment: &Attachment) -> Result<Vec<u8>> {
        ensure!(attachment.is_image(), "Not an image attachment");
        let base = reqwest::Url::parse(&self.account.base_url)?;
        let url = reqwest::Url::parse(&attachment.content_url)
            .context("Invalid attachment URL")?;
        ensure!(url.scheme() == "https"
            && url.host_str() == base.host_str()
            && url.port_or_known_default() == base.port_or_known_default(),
            "Attachment URL is not on the Jira server");
        let response = self.authorize(self.http.get(url))
            .send().context("Could not load Jira attachment")?;
        ensure!(response.status().is_success(),
            "Jira attachment HTTP {}", response.status());
        const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;
        let mut bytes = Vec::new();
        let mut stream = response.take(MAX_IMAGE_BYTES as u64 + 1);
        stream.read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= MAX_IMAGE_BYTES, "Jira image exceeds the preview size limit");
        Ok(bytes)
    }

    /// Fetch complete comments only for the selected issue, not on board loads.
    /// Jira Cloud returns ADF; Jira Server may use plain-text bodies.
    pub fn issue_comments(&self, key: &str) -> Result<Vec<IssueComment>> {
        let version = if self.account.mode == AuthMode::Cloud { "3" } else { "2" };
        let path = format!("/rest/api/{version}/issue/{key}/comment");
        let mut start_at = 0usize;
        let mut comments = Vec::new();
        loop {
            let page = self.read_json(self.http.get(self.url(&path))
                .query(&[("startAt", start_at), ("maxResults", PAGE_SIZE)]))?;
            let values = page.get("comments").and_then(Value::as_array)
                .context("Jira returned an invalid comments response")?;
            let received = values.len();
            comments.extend(values.iter().filter_map(parse_issue_comment));
            start_at += received;
            if received == 0
                || page.get("total").and_then(Value::as_u64)
                    .is_some_and(|total| start_at as u64 >= total)
            {
                break;
            }
        }
        comments.sort_by(|left, right| right.created.cmp(&left.created));
        Ok(comments)
    }

    // Older Jira Server projects use the custom Epic Link field instead of parent.
    // Field IDs vary between instances, so never hardcode customfield_10014.
    fn legacy_epic_link_field(&self) -> Option<String> {
        if self.account.mode != AuthMode::Server {
            return None;
        }
        let fields = self.read_json(self.http.get(self.url("/rest/api/2/field"))).ok()?;
        fields.as_array()?.iter()
            .find(|field| is_legacy_epic_link(field))
            .and_then(|field| field.get("id").and_then(Value::as_str))
            .map(str::to_owned)
    }

    fn resolve_epic_summaries(&self, issues: &mut [Issue]) {
        let mut known: HashMap<String, String> = issues.iter()
            .filter(|issue| issue.is_epic)
            .map(|issue| (issue.key.clone(), issue.summary.clone()))
            .collect();
        let mut unresolved: Vec<String> = issues.iter()
            .filter_map(|issue| issue.epic.as_ref())
            .filter(|epic| epic.summary.is_empty() && !known.contains_key(&epic.key))
            .filter(|epic| validate_project_key(&epic.key).is_ok())
            .map(|epic| epic.key.clone())
            .collect();
        unresolved.sort();
        unresolved.dedup();

        // The user's issue scope often excludes Epics assigned to someone else.
        // Fetch missing titles separately, without changing the visible issue scope.
        for chunk in unresolved.chunks(50) {
            let jql = format!("key IN ({})", chunk.join(", "));
            if let Ok(found) = self.search_issues(&jql) {
                for issue in found.into_iter().filter(|issue| issue.is_epic) {
                    known.insert(issue.key, issue.summary);
                }
            }
        }
        for issue in issues {
            if let Some(epic) = &mut issue.epic
                && epic.summary.is_empty()
                && let Some(summary) = known.get(&epic.key)
            {
                epic.summary.clone_from(summary);
            }
        }
    }

    /// Fetch pinned issues independently of the board's assignee and sprint filters.
    pub fn issues_by_keys(&self, keys: &[String]) -> Result<Vec<Issue>> {
        if keys.is_empty() { return Ok(Vec::new()); }
        let jql = pinned_issues_jql(keys)?;
        let epic_link_field = self.legacy_epic_link_field();
        let mut issues = self.search_issues_with_epic_link(&jql, epic_link_field.as_deref())?;
        self.resolve_epic_summaries(&mut issues);
        Ok(issues)
    }

    pub fn project_issues(&self, keys: &[String], scope: Option<&str>, active_sprint_only: bool) -> Result<Vec<Issue>> {
        if keys.is_empty() { return Ok(Vec::new()); }
        let jql = project_issues_jql(keys, scope, active_sprint_only)?;
        let epic_link_field = self.legacy_epic_link_field();
        let mut issues = self.search_issues_with_epic_link(&jql, epic_link_field.as_deref())?;
        self.resolve_epic_summaries(&mut issues);
        Ok(issues)
    }

}

fn child_issues_jql(key: &str) -> Result<String> {
    let (project, number) = key.split_once('-').context("Invalid parent issue key")?;
    validate_project_key(project)?;
    ensure!(
        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()),
        "Invalid parent issue key"
    );
    Ok(format!("parent = {key} ORDER BY updated DESC"))
}

/// Status-only Jira response parser. Missing or invisible issues are absent,
/// never mistaken for a specific workflow state.
fn parse_issue_statuses(data: &Value) -> HashMap<String, String> {
    data.get("issues").and_then(Value::as_array)
        .into_iter().flatten()
        .filter_map(|issue| {
            let key = issue.get("key")?.as_str()?.to_ascii_uppercase();
            let name = issue.get("fields")?.get("status")?.get("name")?.as_str()?;
            (!name.is_empty()).then_some((key, name.to_owned()))
        })
        .collect()
}

fn pinned_issues_jql(keys: &[String]) -> Result<String> {
    for key in keys {
        let (project, number) = key.rsplit_once('-')
            .context("Invalid pinned Jira issue key")?;
        validate_project_key(project)?;
        ensure!(
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()),
            "Invalid pinned Jira issue key"
        );
    }
    Ok(format!("key IN ({}) ORDER BY updated DESC", keys.join(", ")))
}

fn project_issues_jql(keys: &[String], scope: Option<&str>, active_sprint_only: bool) -> Result<String> {
    for key in keys { validate_project_key(key)?; }
    let projects = keys.join(", ");
    let mut jql = format!("project IN ({projects})");
    if let Some(scope) = scope {
        jql.push_str(" AND (");
        jql.push_str(scope);
        jql.push(')');
    }
    if active_sprint_only {
        jql.push_str(" AND sprint IN openSprints()");
    }
    jql.push_str(" ORDER BY updated DESC");
    Ok(jql)
}

/// Derive ordering from actual workflow transitions; no status-name guesses.
/// When transitions do not give a total order, use the workflow diagram's x layout.
fn parse_workflow_status_order(
    response: &Value,
) -> std::collections::HashMap<String, usize> {
    use std::collections::{HashMap, HashSet, VecDeque};

    let references: HashMap<&str, &str> = response.get("statuses")
        .and_then(Value::as_array).into_iter().flatten()
        .filter_map(|status| Some((
            status.get("statusReference")?.as_str()?,
            status.get("id")?.as_str()?,
        )))
        .collect();
    let mut result = HashMap::new();
    let Some(workflows) = response.get("values").and_then(Value::as_array)
        .or_else(|| response.get("workflows").and_then(Value::as_array))
    else { return result; };

    for workflow in workflows {
        let Some(workflow_statuses) = workflow.get("statuses").and_then(Value::as_array)
        else { continue; };
        let mut layout: Vec<(String, f64)> = workflow_statuses.iter()
            .filter(|status| status.get("deprecated").and_then(Value::as_bool) != Some(true))
            .filter_map(|status| Some((
                status.get("statusReference")?.as_str()?.to_owned(),
                status.get("layout").and_then(|v| v.get("x"))
                    .and_then(Value::as_f64).unwrap_or(f64::MAX),
            )))
            .collect();
        layout.sort_by(|a, b| a.1.total_cmp(&b.1));
        let known: HashSet<_> = layout.iter().map(|(reference, _)| reference.as_str()).collect();
        let mut incoming = HashSet::new();
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        let mut initial = Vec::new();

        for transition in workflow.get("transitions").and_then(Value::as_array)
            .into_iter().flatten()
        {
            let Some(to) = transition.get("toStatusReference").and_then(Value::as_str)
            else { continue; };
            if !known.contains(to) { continue; }
            match transition.get("type").and_then(Value::as_str) {
                Some("INITIAL") => initial.push(to.to_owned()),
                Some("DIRECTED") => {
                    for link in transition.get("links").and_then(Value::as_array)
                        .into_iter().flatten()
                    {
                        if let Some(from) = link.get("fromStatusReference").and_then(Value::as_str)
                            && known.contains(from)
                            && from != to
                        {
                            graph.entry(from.to_owned()).or_default().push(to.to_owned());
                            incoming.insert(to.to_owned());
                        }
                    }
                }
                _ => {}
            }
        }
        if initial.is_empty() {
            initial.extend(layout.iter()
                .filter(|(status, _)| !incoming.contains(status))
                .map(|(status, _)| status.clone()));
        }
        if initial.is_empty() {
            initial.extend(layout.first().map(|(status, _)| status.clone()));
        }
        let mut distances = HashMap::new();
        let mut queue = VecDeque::new();
        for root in initial {
            if distances.insert(root.clone(), 0usize).is_none() {
                queue.push_back(root);
            }
        }
        while let Some(current) = queue.pop_front() {
            let distance = distances[&current];
            for next in graph.get(&current).into_iter().flatten() {
                if !distances.contains_key(next) {
                    distances.insert(next.clone(), distance + 1);
                    queue.push_back(next.clone());
                }
            }
        }
        layout.sort_by(|a, b| {
            distances.get(&a.0).copied().unwrap_or(usize::MAX)
                .cmp(&distances.get(&b.0).copied().unwrap_or(usize::MAX))
                .then_with(|| a.1.total_cmp(&b.1))
        });
        for (position, (reference, _)) in layout.iter().enumerate() {
            if let Some(id) = references.get(reference.as_str()) {
                result.entry((*id).to_owned())
                    .and_modify(|old: &mut usize| *old = (*old).min(position))
                    .or_insert(position);
            }
        }
    }
    result
}

fn parse_project(data: &Value) -> Option<Project> {
    Some(Project {
        key: data.get("key")?.as_str()?.to_owned(),
        name: data.get("name")?.as_str()?.to_owned(),
    })
}

fn validate_project_key(key: &str) -> Result<()> {
    ensure!(!key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "Invalid Jira project key: {key}");
    Ok(())
}

fn parse_issues(data: &Value) -> Vec<Issue> {
    parse_issues_with_epic_link(data, None)
}

fn parse_issues_with_epic_link(data: &Value, epic_link_field: Option<&str>) -> Vec<Issue> {
    data.get("issues").and_then(Value::as_array)
        .into_iter().flatten()
        .filter_map(|value| parse_issue(value, epic_link_field))
        .collect()
}

fn is_legacy_epic_link(field: &Value) -> bool {
    // The display name can be changed, localized, or reused by another field.
    // GreenHopper's exact custom type identifies the real Epic Link field.
    field.get("schema").and_then(|schema| schema.get("custom"))
        .and_then(Value::as_str)
        == Some("com.pyxis.greenhopper.jira:gh-epic-link")
}

fn parse_issue(v: &Value, epic_link_field: Option<&str>) -> Option<Issue> {
    let key = v.get("key")?.as_str()?.to_owned();
    let f = v.get("fields")?;
    let field = |name: &str| f.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    let nested = |key: &str, name: &str| f.get(key).and_then(|v| v.get(name))
        .and_then(Value::as_str).unwrap_or("").to_owned();
    let kind = nested("issuetype", "name");
    let is_epic = kind.eq_ignore_ascii_case("epic")
        || f.get("issuetype").and_then(|kind| kind.get("hierarchyLevel"))
            .and_then(Value::as_i64) == Some(1);
    let parent = f.get("parent").and_then(|parent| Some(IssueParent {
        key: parent.get("key")?.as_str()?.to_owned(),
        summary: parent.get("fields").and_then(|fields| fields.get("summary"))
            .and_then(Value::as_str).unwrap_or("").to_owned(),
    }));
    let epic = f.get("parent")
        .filter(|parent| {
            let kind = parent.get("fields").and_then(|fields| fields.get("issuetype"));
            kind.and_then(|kind| kind.get("hierarchyLevel"))
                .and_then(Value::as_i64) == Some(1)
                || kind.and_then(|kind| kind.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case("epic"))
        })
        .and_then(|parent| Some(Epic {
            key: parent.get("key")?.as_str()?.to_owned(),
            summary: parent.get("fields").and_then(|fields| fields.get("summary"))
                .and_then(Value::as_str).unwrap_or("").to_owned(),
        }))
        .or_else(|| {
            let key = f.get(epic_link_field?)?.as_str()?;
            Some(Epic { key: key.to_owned(), summary: String::new() })
        });
    Some(Issue {
        project_key: f.get("project").and_then(|p| p.get("key"))
            .and_then(Value::as_str).map(str::to_owned)
            .unwrap_or_else(|| key.split('-').next().unwrap_or("").to_owned()),
        key,
        summary: field("summary"),
        status: nested("status", "name"),
        status_id: nested("status", "id"),
        status_category: f.get("status").and_then(|status| status.get("statusCategory"))
            .and_then(parse_status_category),
        kind,
        priority: nested("priority", "name"),
        fix_versions: f.get("fixVersions").and_then(Value::as_array)
            .into_iter().flatten()
            .filter_map(|version| version.get("name").and_then(Value::as_str))
            .map(str::to_owned).collect(),
        affects_versions: f.get("versions").and_then(Value::as_array)
            .into_iter().flatten()
            .filter_map(|version| version.get("name").and_then(Value::as_str))
            .map(str::to_owned).collect(),
        assignee: nested("assignee", "displayName"),
        updated: field("updated").chars().take(10).collect(),
        description: format_description(f.get("description")),
        description_doc: f.get("description").filter(|value| value.is_object()).cloned(),
        attachments: f.get("attachment").and_then(Value::as_array)
            .into_iter().flatten().filter_map(|a| Some(Attachment {
                id: a.get("id")?.as_str()?.to_owned(),
                filename: a.get("filename")?.as_str()?.to_owned(),
                mime_type: a.get("mimeType").and_then(Value::as_str).unwrap_or("").to_owned(),
                content_url: a.get("content").and_then(Value::as_str).unwrap_or("").to_owned(),
            })).collect(),
        is_epic,
        epic,
        parent,
    })
}

fn parse_issue_comment(value: &Value) -> Option<IssueComment> {
    Some(IssueComment {
        author: value.get("author")
            .and_then(|author| author.get("displayName").or_else(|| author.get("name")))
            .and_then(Value::as_str).unwrap_or("Unknown user").to_owned(),
        created: value.get("created").and_then(Value::as_str)
            .unwrap_or("").to_owned(),
        body: value.get("body")?.clone(),
    })
}

fn format_description(value: Option<&Value>) -> String {
    let Some(value) = value else { return String::new(); };
    if let Some(text) = value.as_str() { return text.to_owned(); }
    let mut output = String::new();
    append_adf(value, &mut output);
    output.trim().to_owned()
}

fn append_adf(node: &Value, out: &mut String) {
    if let Some(text) = node.get("text").and_then(Value::as_str) {
        out.push_str(text);
    }
    if node.get("type").and_then(Value::as_str) == Some("hardBreak") {
        out.push('\n');
    }
    if let Some(items) = node.get("content").and_then(Value::as_array) {
        for item in items {
            append_adf(item, out);
        }
    }
    if matches!(node.get("type").and_then(Value::as_str),
        Some("paragraph" | "heading" | "listItem" | "bulletList" | "orderedList" | "blockquote" | "codeBlock")) {
        out.push('\n');
    }
}


fn field_has_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) if values.get("type").and_then(Value::as_str) == Some("doc") => {
            values.get("content").and_then(Value::as_array)
                .is_some_and(|content| content.iter().any(adf_node_has_value))
        }
        Value::Object(values) => !values.is_empty(),
        _ => true,
    }
}

fn adf_node_has_value(node: &Value) -> bool {
    if node.get("text").and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
    {
        return true;
    }
    // ADF nodes such as images, mentions and cards are content even if
    // they do not contain a text leaf.
    if matches!(
        node.get("type").and_then(Value::as_str),
        Some("media" | "mediaInline" | "mention" | "emoji" | "inlineCard" | "blockCard")
    ) {
        return true;
    }
    node.get("content").and_then(Value::as_array)
        .is_some_and(|content| content.iter().any(adf_node_has_value))
}

fn remove_populated_fields(
    transition: &mut IssueTransition,
    values: &serde_json::Map<String, Value>,
) {
    // Worklog is an action, not a value that can be satisfied by a prior log.
    // Other already-set fields are left unchanged and excluded from the form.
    transition.fields.retain(|field| {
        field.is_worklog() || !values.get(&field.id).is_some_and(field_has_value)
    });
    transition.edit_fields.retain(|field| {
        field.is_worklog() || !values.get(&field.id).is_some_and(field_has_value)
    });
}

fn parse_transition_fields(metadata_fields: &serde_json::Map<String, Value>) -> Vec<TransitionField> {
    let mut fields = Vec::new();
    for (id, metadata) in metadata_fields {
        let required = metadata.get("required").and_then(Value::as_bool) == Some(true)
            && metadata.get("hasDefaultValue").and_then(Value::as_bool) != Some(true);
        let schema = metadata.get("schema");
        let schema_type = schema.and_then(|s| s.get("type"))
            .and_then(Value::as_str).unwrap_or("").to_owned();
        let item_type = schema.and_then(|s| s.get("items"))
            .and_then(Value::as_str).unwrap_or("").to_owned();
        let custom_type = schema.and_then(|s| s.get("custom"))
            .and_then(Value::as_str).unwrap_or("").to_owned();
        let choices = metadata.get("allowedValues").and_then(Value::as_array)
            .into_iter().flatten()
            .filter_map(|value| {
                let label = value.as_str()
                    .or_else(|| value.get("name").and_then(Value::as_str))
                    .or_else(|| value.get("value").and_then(Value::as_str))
                    .or_else(|| value.get("displayName").and_then(Value::as_str))
                    .or_else(|| value.get("key").and_then(Value::as_str))?
                    .to_owned();
                let payload = if let Some(id) = value.get("id") {
                    json!({"id": id})
                } else if let Some(id) = value.get("accountId") {
                    json!({"accountId": id})
                } else if let Some(name) = value.get("name") {
                    json!({"name": name})
                } else {
                    value.clone()
                };
                Some(TransitionChoice { label, value: payload })
            }).collect();
        fields.push(TransitionField {
            id: id.clone(),
            name: metadata.get("name").and_then(Value::as_str)
                .unwrap_or(id).to_owned(),
            schema_type,
            item_type,
            custom_type,
            required,
            choices,
        });
    }
    fields.sort_by(|a, b| b.required.cmp(&a.required)
        .then_with(|| a.name.cmp(&b.name)));
    fields
}

fn parse_issue_transitions(response: &Value) -> Vec<IssueTransition> {
    response.get("transitions").and_then(Value::as_array)
        .into_iter().flatten().filter_map(|transition| {
            let id = transition.get("id")?.as_str()?.to_owned();
            let name = transition.get("name")?.as_str()?.to_owned();
            let to = transition.get("to")?;
            let to_name = to.get("name").and_then(Value::as_str).unwrap_or(&name).to_owned();
            let to_id = to.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
            let to_category = to.get("statusCategory").and_then(parse_status_category);
            let is_global = transition.get("isGlobal").and_then(Value::as_bool);
            let opsbar_sequence = transition.get("opsbarSequence").and_then(Value::as_i64);
            let fields = transition.get("fields").and_then(Value::as_object)
                .map(parse_transition_fields).unwrap_or_default();
            Some(IssueTransition {
                id, name, to_name, to_id, to_category, is_global, opsbar_sequence,
                fields, edit_fields: Vec::new(),
            })
        }).collect()
}

fn transition_payload(transition_id: &str, mut fields: serde_json::Map<String, Value>) -> Value {
    let comment = fields.remove("comment");
    let mut payload = json!({"transition": {"id": transition_id}, "fields": fields});
    // Jira handles comments through update operations, not the issue's fields.
    if let Some(body) = comment {
        payload["update"] = json!({"comment": [{"add": {"body": body}}]});
    }
    payload
}

fn jira_error_message(body: &str) -> String {
    let Ok(response) = serde_json::from_str::<Value>(body) else {
        return if body.trim().is_empty() { "Transition failed".to_owned() } else { body.to_owned() };
    };
    let mut messages: Vec<String> = response.get("errorMessages").and_then(Value::as_array)
        .into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    if let Some(errors) = response.get("errors").and_then(Value::as_object) {
        for (field, error) in errors {
            if let Some(error) = error.as_str() {
                messages.push(format!("{field}: {error}"));
            }
        }
    }
    if messages.is_empty() { "Transition failed".to_owned() } else { messages.join("; ") }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pinned_query_ignores_board_filters_and_rejects_invalid_keys() {
        assert_eq!(
            pinned_issues_jql(&["ART-42".into(), "AFB-2".into()]).unwrap(),
            "key IN (ART-42, AFB-2) ORDER BY updated DESC"
        );
        assert!(pinned_issues_jql(&["ART-42) OR assignee IS EMPTY".into()]).is_err());
        assert!(pinned_issues_jql(&["ART-nope".into()]).is_err());
    }

    use super::*;

    #[test]
    fn reads_current_statuses_without_guessing_for_missing_issues() {
        let response = serde_json::json!({
            "issues": [
                {"key": "art-2272", "fields": {"status": {"name": "Ready to Dev"}}},
                {"key": "AFB-29", "fields": {"status": {"name": "In Progress"}}},
                {"key": "AFB-30", "fields": {"status": null}},
                {"key": "ART-1", "fields": {}}
            ]
        });
        let statuses = parse_issue_statuses(&response);
        assert_eq!(statuses.get("ART-2272"), Some(&"Ready to Dev".to_owned()));
        assert_eq!(statuses.get("AFB-29"), Some(&"In Progress".to_owned()));
        assert!(!statuses.contains_key("AFB-30"));
        assert!(!statuses.contains_key("ART-1"));
        assert_eq!(statuses.len(), 2);
    }

    #[test]
    fn project_jql_combines_sprint_and_assignee_filters() {
        let projects = vec!["ART".to_owned(), "AFB".to_owned()];
        assert_eq!(
            project_issues_jql(&projects, Some("assignee = currentUser()"), true).unwrap(),
            "project IN (ART, AFB) AND (assignee = currentUser()) AND sprint IN openSprints() ORDER BY updated DESC"
        );
        assert_eq!(
            project_issues_jql(&projects, Some("assignee IS EMPTY"), true).unwrap(),
            "project IN (ART, AFB) AND (assignee IS EMPTY) AND sprint IN openSprints() ORDER BY updated DESC"
        );
        assert_eq!(
            project_issues_jql(&projects, None, true).unwrap(),
            "project IN (ART, AFB) AND sprint IN openSprints() ORDER BY updated DESC"
        );
        assert_eq!(
            project_issues_jql(&projects, Some("assignee = currentUser()"), false).unwrap(),
            "project IN (ART, AFB) AND (assignee = currentUser()) ORDER BY updated DESC"
        );
    }

    #[test]
    fn rejects_http_and_paths() {
        let mut c = Credentials {base_url:"http://jira.example.com".into(),mode:AuthMode::Server,email:String::new(),token:"x".into()};
        assert!(c.validate().is_err());
        c.base_url = "https://jira.example.com/?q=secret".into();
        assert!(c.validate().is_err());
        c.base_url = "https://jira.example.com/".into();
        assert!(c.validate().is_ok());
        assert_eq!(c.base_url, "https://jira.example.com");
    }

    #[test]
    fn derives_order_from_transition_graph_even_with_localized_statuses() {
        let response = json!({
            "statuses": [
                {"id": "3", "statusReference": "done", "name": "Fertig"},
                {"id": "1", "statusReference": "todo", "name": "Čaká"},
                {"id": "2", "statusReference": "progress", "name": "Rieši sa"}
            ],
            "values": [{
                "statuses": [
                    {"statusReference": "done", "layout": {"x": 0}},
                    {"statusReference": "progress", "layout": {"x": 300}},
                    {"statusReference": "todo", "layout": {"x": 600}}
                ],
                "transitions": [
                    {"type": "INITIAL", "toStatusReference": "todo"},
                    {"type": "DIRECTED", "toStatusReference": "progress",
                        "links": [{"fromStatusReference": "todo"}]},
                    {"type": "DIRECTED", "toStatusReference": "done",
                        "links": [{"fromStatusReference": "progress"}]},
                    {"type": "DIRECTED", "toStatusReference": "progress",
                        "links": [{"fromStatusReference": "done"}]}
                ]
            }]
        });
        let result = parse_workflow_status_order(&response);
        assert_eq!(result.get("1"), Some(&0));
        assert_eq!(result.get("2"), Some(&1));
        assert_eq!(result.get("3"), Some(&2));
    }

    #[test]
    fn workflow_order_falls_back_to_diagram_layout_without_directed_transitions() {
        let response = json!({
            "statuses": [
                {"id": "20", "statusReference": "b"},
                {"id": "10", "statusReference": "a"}
            ],
            "values": [{
                "statuses": [
                    {"statusReference": "b", "layout": {"x": 300}},
                    {"statusReference": "a", "layout": {"x": 100}}
                ],
                "transitions": []
            }]
        });
        let result = parse_workflow_status_order(&response);
        assert_eq!(result.get("10"), Some(&0));
        assert_eq!(result.get("20"), Some(&1));
    }

    #[test]
    fn parses_jira_status_category_formats() {
        for (value, expected) in [
            (json!({"key": "new"}), StatusCategory::Todo),
            (json!({"key": "indeterminate"}), StatusCategory::InProgress),
            (json!({"key": "done"}), StatusCategory::Done),
            (json!("TODO"), StatusCategory::Todo),
            (json!("IN_PROGRESS"), StatusCategory::InProgress),
            (json!("DONE"), StatusCategory::Done),
        ] {
            assert_eq!(parse_status_category(&value), Some(expected));
        }
        assert_eq!(parse_status_category(&json!({})), None);
    }

    #[test]
    fn retains_attachment_metadata_for_lazy_previews() {
        let data = json!({"issues":[{"key":"ART-10","fields":{
            "summary":"Illustrated description",
            "description":{"type":"doc","content":[{"type":"mediaSingle","content":[{"type":"media","attrs":{"id":"uuid-image","type":"file"}}]}]},
            "attachment":[
                {"id":"42","filename":"screen.png","mimeType":"image/png","content":"https://jira.example.com/rest/api/3/attachment/content/42"},
                {"id":"43","filename":"notes.txt","mimeType":"text/plain","content":"https://jira.example.com/rest/api/3/attachment/content/43"}
            ]
        }}]});
        let issue = &parse_issues(&data)[0];
        assert_eq!(issue.attachments.len(), 2);
        assert!(issue.attachments[0].is_image());
        assert!(!issue.attachments[1].is_image());
        assert_eq!(issue.description_doc.as_ref().unwrap()["content"][0]["type"], "mediaSingle");
    }

    #[test]
    fn recognizes_epic_parents_but_not_subtask_parents() {
        let data = json!({"issues":[
            {"key":"ART-1","fields":{
                "summary":"An Epic","issuetype":{"name":"Initiative","hierarchyLevel":1}
            }},
            {"key":"ART-2","fields":{
                "summary":"Story","issuetype":{"name":"Story","hierarchyLevel":0},
                "parent":{"key":"ART-1","fields":{
                    "summary":"An Epic","issuetype":{"name":"Initiative","hierarchyLevel":1}
                }}
            }},
            {"key":"ART-3","fields":{
                "summary":"Subtask","issuetype":{"name":"Sub-task","hierarchyLevel":-1},
                "parent":{"key":"ART-2","fields":{
                    "summary":"Story","issuetype":{"name":"Story","hierarchyLevel":0}
                }}
            }}
        ]});
        let issues = parse_issues(&data);
        assert!(issues[0].is_epic);
        assert!(!issues[1].is_epic);
        assert_eq!(issues[1].epic, Some(Epic {
            key: "ART-1".into(), summary: "An Epic".into()
        }));
        assert!(issues[2].epic.is_none());
        assert_eq!(issues[1].parent, Some(IssueParent {
            key: "ART-1".into(), summary: "An Epic".into()
        }));
        assert_eq!(issues[2].parent, Some(IssueParent {
            key: "ART-2".into(), summary: "Story".into()
        }));
    }

    #[test]
    fn child_query_is_scoped_only_by_the_parent_issue() {
        assert_eq!(child_issues_jql("ART-2272").unwrap(),
            "parent = ART-2272 ORDER BY updated DESC");
        assert!(child_issues_jql("ART-1 OR assignee = currentUser()").is_err());
        assert!(child_issues_jql("ART-ab").is_err());
    }

    #[test]
    fn reads_legacy_server_epic_link_by_discovered_field_id() {
        let data = json!({"issues":[{"key":"ART-4","fields":{
            "summary":"Bug","issuetype":{"name":"Bug"},
            "customfield_12345":"ART-1"
        }}]});
        let issues = parse_issues_with_epic_link(&data, Some("customfield_12345"));
        assert_eq!(issues[0].epic, Some(Epic {
            key: "ART-1".into(), summary: String::new()
        }));
        assert!(parse_issues(&data)[0].epic.is_none());
    }

    #[test]
    fn reads_fix_versions_including_empty_and_missing_values() {
        let data = json!({"issues":[
            {"key":"ART-101","fields":{"fixVersions":[
                {"id":"1","name":"1.2.0"},
                {"id":"2","name":"1.3.0"}
            ]}},
            {"key":"ART-102","fields":{"fixVersions":[{"name":"2.0"}]}},
            {"key":"ART-103","fields":{"fixVersions":[]}},
            {"key":"ART-104","fields":{"fixVersions":null}},
            {"key":"ART-105","fields":{}}
        ]});
        assert!(FIELDS.contains(&"fixVersions"));
        let issues = parse_issues(&data);
        assert_eq!(issues[0].fix_versions, vec!["1.2.0", "1.3.0"]);
        assert_eq!(issues[1].fix_versions, vec!["2.0"]);
        for issue in issues.iter().skip(2) {
            assert!(issue.fix_versions.is_empty());
        }
    }

    #[test]
    fn reads_affects_versions_including_empty_and_missing_values() {
        let data = json!({"issues":[
            {"key":"ART-201","fields":{
                "versions":[{"id":"1","name":"1.2.0"},{"id":"2","name":"1.3.0"}],
                "fixVersions":[{"name":"1.4.0"}]
            }},
            {"key":"ART-202","fields":{"versions":[{"name":"2.0"}]}},
            {"key":"ART-203","fields":{"versions":[]}},
            {"key":"ART-204","fields":{"versions":null}},
            {"key":"ART-205","fields":{}}
        ]});
        assert!(FIELDS.contains(&"versions"));
        let issues = parse_issues(&data);
        assert_eq!(issues[0].affects_versions, vec!["1.2.0", "1.3.0"]);
        assert_eq!(issues[0].fix_versions, vec!["1.4.0"]);
        assert_eq!(issues[1].affects_versions, vec!["2.0"]);
        for issue in issues.iter().skip(2) {
            assert!(issue.affects_versions.is_empty());
        }
    }

    #[test]
    fn parses_comments_with_cloud_adf_and_server_plain_text() {
        let cloud = parse_issue_comment(&json!({
            "id": "42", "author": {"displayName": "Anna"},
            "created": "2026-11-08T13:20:01.000+0100",
            "body": {"type": "doc", "version": 1, "content": [
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "Hello", "marks": [{"type": "strong"}]}
                ]}
            ]}
        })).unwrap();
        assert_eq!(cloud.author, "Anna");
        assert_eq!(cloud.body["content"][0]["content"][0]["text"], "Hello");

        let server = parse_issue_comment(&json!({
            "author": {"name": "legacy-user"},
            "created": "2026-11-01T09:10:00.000+0100",
            "body": "Plain text"
        })).unwrap();
        assert_eq!(server.author, "legacy-user");
        assert_eq!(server.body.as_str(), Some("Plain text"));
        assert!(parse_issue_comment(&json!({"author": {}})).is_none());
    }

    #[test]
    fn reads_cloud_adf_and_status() {
        let data = json!({"issues":[{"key":"ABC-42","fields":{
            "summary":"Fix crash",
            "status":{"id":"3","name":"In Progress"},
            "description":{"type":"doc","content":[
                {"type":"paragraph","content":[{"type":"text","text":"Hello"}]},
                {"type":"paragraph","content":[{"type":"text","text":"World"}]}
            ]}
        }}]});
        let issues = parse_issues(&data);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].status_id, "3");
        assert_eq!(issues[0].description, "Hello\nWorld");
        assert_eq!(issues[0].description_doc.as_ref().and_then(|doc| doc.get("type")).and_then(Value::as_str), Some("doc"));
    }
}


#[cfg(test)]
mod transition_tests {
    use super::*;

    #[test]
    fn parses_all_screen_fields_and_required_flags_from_transition_metadata() {
        let data = json!({"transitions": [{
            "id": "31", "name": "Close", "to": {"id": "10002", "name": "Done"},
            "fields": {
                "resolution": {
                    "required": true, "name": "Resolution",
                    "schema": {"type": "resolution"},
                    "allowedValues": [{"id": "1", "name": "Fixed"}]
                },
                "comment": {"required": false, "schema": {"type": "string"}},
                "customfield_42": {"required": true, "hasDefaultValue": true}
            }
        }]});
        let transitions = parse_issue_transitions(&data);
        assert_eq!(transitions.len(), 1);
        let transition = &transitions[0];
        assert_eq!(transition.to_name, "Done");
        assert_eq!(transition.fields.len(), 3);
        let resolution = transition.fields.iter().find(|field| field.id == "resolution").unwrap();
        assert!(resolution.required);
        assert_eq!(resolution.choices[0].value, json!({"id": "1"}));
        let comment = transition.fields.iter().find(|field| field.id == "comment").unwrap();
        assert!(!comment.required);
        let defaulted = transition.fields.iter().find(|field| field.id == "customfield_42").unwrap();
        assert!(!defaulted.required);
    }

    #[test]
    fn existing_fix_and_affects_versions_are_not_requested_again() {
        let mut transition = IssueTransition {
            id: "31".into(), name: "Review".into(),
            to_name: "Review".into(), to_id: "3".into(), to_category: None,
            is_global: None, opsbar_sequence: None,
            fields: vec![
                TransitionField { id: "fixVersions".into(), name: "Fix versions".into(),
                    schema_type: "array".into(), item_type: "version".into(),
                    custom_type: String::new(), required: true, choices: vec![] },
                TransitionField { id: "versions".into(), name: "Affects versions".into(),
                    schema_type: "array".into(), item_type: "version".into(),
                    custom_type: String::new(), required: true, choices: vec![] },
                TransitionField { id: "customfield_123".into(), name: "Impact assessment".into(),
                    schema_type: "string".into(), item_type: String::new(),
                    custom_type: String::new(), required: false, choices: vec![] },
            ],
            edit_fields: vec![],
        };
        let values = json!({
            "fixVersions": [{"id":"14070","name":"19.2 Bugfix 2"}],
            "versions": [{"id":"10000","name":"15.0.0"}],
            "customfield_123": null
        });
        remove_populated_fields(&mut transition, values.as_object().unwrap());
        assert_eq!(transition.fields.len(), 1);
        assert_eq!(transition.fields[0].id, "customfield_123");
    }

    #[test]
    fn log_work_is_not_satisfied_by_an_older_worklog() {
        let mut transition = IssueTransition {
            id: "31".into(), name: "Review".into(),
            to_name: "Review".into(), to_id: "3".into(), to_category: None,
            is_global: None, opsbar_sequence: None,
            fields: vec![TransitionField {
                id: "worklog".into(), name: "Log work".into(), required: true,
                schema_type: "array".into(), item_type: "worklog".into(),
                custom_type: String::new(), choices: vec![],
            }],
            edit_fields: vec![],
        };
        remove_populated_fields(&mut transition,
            json!({"worklog": [{"id":"99"}]}).as_object().unwrap());
        assert_eq!(transition.fields.len(), 1);
    }

    #[test]
    fn legacy_epic_link_requires_exact_custom_field_type() {
        assert!(is_legacy_epic_link(&json!({
            "name": "Different localized name",
            "schema": {"custom":"com.pyxis.greenhopper.jira:gh-epic-link"}
        })));
        assert!(!is_legacy_epic_link(&json!({
            "name": "Epic Link",
            "schema": {"custom":"vendor.example:some-other-field"}
        })));
        assert!(!is_legacy_epic_link(&json!({
            "name": "Epic Link",
            "schema": {"type":"string"}
        })));
    }

    #[test]
    fn empty_adf_document_does_not_count_as_a_filled_required_field() {
        assert!(!field_has_value(&json!({"type":"doc","version":1,"content":[]})));
        assert!(!field_has_value(&json!({
            "type":"doc", "version":1,
            "content":[{"type":"paragraph","content":[]}]
        })));
        assert!(field_has_value(&json!({
            "type":"doc", "version":1,
            "content":[{"type":"paragraph","content":[{"type":"text","text":"Impact"}]}]
        })));
        assert!(field_has_value(&json!({
            "type":"doc", "version":1,
            "content":[{"type":"mediaSingle","content":[{"type":"media","attrs":{"id":"123"}}]}]
        })));
    }

    #[test]
    fn recognizes_existing_values_in_required_fields() {
        for value in [json!("Fixed"), json!([{"id": "1"}]), json!({"id": "1"}), json!(4)] {
            assert!(field_has_value(&value));
        }
        for value in [Value::Null, json!(""), json!([]), json!({})] {
            assert!(!field_has_value(&value));
        }
    }

    #[test]
    fn validates_and_serializes_transition_inputs() {
        let text = TransitionField {
            id: "description".into(), name: "Description".into(),
            schema_type: "string".into(), item_type: String::new(),
            custom_type: String::new(), required: true, choices: vec![],
        };
        assert_eq!(text.text_value("Hello", false).unwrap(), json!("Hello"));
        assert_eq!(text.text_value("Hello", true).unwrap()["type"], "doc");
        let date = TransitionField {
            schema_type: "date".into(), ..text.clone()
        };
        assert!(date.text_value("2026-11-06", true).is_ok());
        assert!(date.text_value("06/11/2026", true).is_err());
        let number = TransitionField {
            schema_type: "number".into(), ..text
        };
        assert_eq!(number.text_value("2.5", false).unwrap(), json!(2.5));
        assert!(number.text_value("invalid", false).is_err());
    }

    #[test]
    fn finds_editable_fields_missing_from_transition_screen() {
        let transitions = parse_issue_transitions(&json!({"transitions":[
            {"id":"3","name":"Done","to":{"name":"Done"},
             "fields":{"resolution":{"name":"Resolution","required":false,
                 "schema":{"type":"resolution"}}}}
        ]}));
        let edit = parse_transition_fields(json!({
            "customfield_42": {
                "name":"Business justification", "required":false,
                "schema":{"type":"string"}
            },
            "resolution": {
                "name":"Resolution", "required":false,
                "schema":{"type":"resolution"}
            }
        }).as_object().unwrap());
        let available: Vec<_> = edit.iter().filter(|field| {
            !transitions[0].fields.iter().any(|screen_field| screen_field.id == field.id)
        }).collect();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, "customfield_42");
    }

    #[test]
    fn transition_comment_is_sent_as_an_update_operation() {
        let mut fields = serde_json::Map::new();
        fields.insert("comment".into(), json!("Done"));
        fields.insert("resolution".into(), json!({"id":"1"}));
        let body = transition_payload("31", fields);
        assert_eq!(body["transition"]["id"], "31");
        assert_eq!(body["fields"]["resolution"]["id"], "1");
        assert!(body["fields"].get("comment").is_none());
        assert_eq!(body["update"]["comment"][0]["add"]["body"], "Done");
    }

    #[test]
    fn parses_jira_workflow_priority_without_matching_transition_names() {
        let data = json!({"transitions": [
            {"id":"12", "name":"Whatever", "isGlobal":false, "opsbarSequence":20,
                "to":{"id":"4", "name":"B"}},
            {"id":"13", "name":"Anything", "isGlobal":true, "opsbarSequence":10,
                "to":{"id":"5", "name":"C"}}
        ]});
        let transitions = parse_issue_transitions(&data);
        assert_eq!(transitions[0].is_global, Some(false));
        assert_eq!(transitions[0].opsbar_sequence, Some(20));
        assert_eq!(transitions[1].is_global, Some(true));
        assert_eq!(transitions[1].opsbar_sequence, Some(10));
    }

    #[test]
    fn parses_transition_with_no_fields_for_confirmation_dialog() {
        let data = json!({"transitions": [{
            "id":"2", "name":"In Progress", "to":{"name":"In Progress", "id":"10"}
        }]});
        let transitions = parse_issue_transitions(&data);
        assert_eq!(transitions.len(), 1);
        assert!(transitions[0].fields.is_empty());
    }

    #[test]
    fn uses_machine_field_keys_instead_of_error_message_guesses() {
        let structured = JiraTransitionError::from_response(
            400,
            r#"{"errorMessages":["Please fill impact assessment and fix versions"],"errors":{"customfield_10135":"Required","fixVersions":"Required"}}"#,
        );
        assert_eq!(structured.field_keys, vec!["customfield_10135", "fixVersions"]);
        assert!(structured.is_validation_failure());

        let unstructured = JiraTransitionError::from_response(
            400, r#"{"errorMessages":["Please fill impact assessment and fix versions"]}"#,
        );
        assert!(unstructured.field_keys.is_empty());
        assert!(unstructured.is_validation_failure());
        assert!(!JiraTransitionError::from_response(403, "Forbidden").is_validation_failure());
    }

    #[test]
    fn exposes_field_specific_jira_validation_errors() {
        let error = jira_error_message(r#"{"errors":{"resolution":"Required","customfield_3":"Invalid"}}"#);
        assert!(error.contains("resolution: Required"));
        assert!(error.contains("customfield_3: Invalid"));
    }
}


#[cfg(test)]
mod advanced_transition_tests {
    use super::*;

    fn field(schema_type: &str, item_type: &str) -> TransitionField {
        TransitionField {
            id: "customfield_123".into(), name: "Example".into(),
            schema_type: schema_type.into(), item_type: item_type.into(),
            custom_type: String::new(), required: true, choices: vec![],
        }
    }

    #[test]
    fn supports_jira_object_and_multiselect_fields() {
        assert_eq!(
            field("resolution", "").text_value("10000", true).unwrap(),
            json!({"id":"10000"})
        );
        assert_eq!(
            field("user", "").text_value("user-123", true).unwrap(),
            json!({"accountId":"user-123"})
        );
        assert_eq!(
            field("user", "").text_value("john", false).unwrap(),
            json!({"name":"john"})
        );
        assert_eq!(
            field("array", "option").text_value("10, 20", true).unwrap(),
            json!([{"id":"10"}, {"id":"20"}])
        );
    }

    #[test]
    fn complex_custom_field_accepts_valid_json() {
        let field = field("any", "");
        assert!(field.uses_structured_input());
        assert_eq!(
            field.text_value(r#"{"id":"123","child":{"id":"456"}}"#, true).unwrap(),
            json!({"id":"123","child":{"id":"456"}})
        );
        assert!(field.text_value("{bad", true).is_err());
        assert!(field.text_value("null", true).is_err());
    }

    #[test]
    fn custom_textarea_uses_cloud_adf() {
        let mut textarea = field("string", "");
        textarea.custom_type = "com.atlassian.jira.plugin.system.customfieldtypes:textarea".into();
        assert!(textarea.is_multiline());
        assert_eq!(textarea.text_value("A note", true).unwrap()["type"], "doc");
        assert_eq!(textarea.text_value("A note", false).unwrap(), json!("A note"));
    }
    #[test]
    fn dev_status_parses_pull_request_links_and_states() {
        let response = json!({
            "detail": [{
                "pullRequests": [{
                    "id": "12555",
                    "name": "AFB-29 NQD: Add components to questionnaires",
                    "url": "https://dev.azure.com/resconet/MobileCRM/_git/web-projects-nx/pullrequest/12555",
                    "status": "MERGED"
                }]
            }]
        });
        assert_eq!(parse_dev_pull_requests(&response), vec![PullRequest {
            title: "AFB-29 NQD: Add components to questionnaires".into(),
            url: "https://dev.azure.com/resconet/MobileCRM/_git/web-projects-nx/pullrequest/12555".into(),
            status: "MERGED".into(),
        }]);
    }

    #[test]
    fn dev_status_accepts_plural_details_and_skips_unsafe_links() {
        let response = json!({"details": [
            {"pullrequests": [
                {"title": "Valid", "url": "https://example.com/pr/2", "state": "open"},
                {"title": "Unsafe", "url": "javascript:alert(1)"}
            ]}
        ]});
        let parsed = parse_dev_pull_requests(&response);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "Valid");
        assert_eq!(parsed[0].status, "OPEN");
    }

}
