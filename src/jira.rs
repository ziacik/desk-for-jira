use std::{collections::{HashMap, HashSet}, io::Read, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PAGE_SIZE: usize = 100;
const MAX_ITEMS: usize = 500;
const FIELDS: [&str; 12] = [
    "summary",
    "status",
    "assignee",
    "issuetype",
    "priority",
    "fixVersions",
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

#[derive(Clone, Debug)]
pub struct Issue {
    pub key: String,
    pub project_key: String,
    pub summary: String,
    pub status: String,
    pub status_id: String,
    pub kind: String,
    pub priority: String,
    pub fix_versions: Vec<String>,
    pub assignee: String,
    pub updated: String,
    pub description: String,
    pub description_doc: Option<Value>,
    pub attachments: Vec<Attachment>,
    pub is_epic: bool,
    pub epic: Option<Epic>,
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

    // Older Jira Server projects use the custom Epic Link field instead of parent.
    // Field IDs vary between instances, so never hardcode customfield_10014.
    fn legacy_epic_link_field(&self) -> Option<String> {
        if self.account.mode != AuthMode::Server {
            return None;
        }
        let fields = self.read_json(self.http.get(self.url("/rest/api/2/field"))).ok()?;
        fields.as_array()?.iter()
            .find(|field| {
                field.get("schema").and_then(|schema| schema.get("custom"))
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.contains("gh-epic-link"))
                    || field.get("name").and_then(Value::as_str)
                        .is_some_and(|name| name.eq_ignore_ascii_case("Epic Link"))
            })
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

    pub fn project_issues(&self, keys: &[String], scope: Option<&str>, active_sprint_only: bool) -> Result<Vec<Issue>> {
        if keys.is_empty() { return Ok(Vec::new()); }
        let jql = project_issues_jql(keys, scope, active_sprint_only)?;
        let epic_link_field = self.legacy_epic_link_field();
        let mut issues = self.search_issues_with_epic_link(&jql, epic_link_field.as_deref())?;
        self.resolve_epic_summaries(&mut issues);
        Ok(issues)
    }

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
        kind,
        priority: nested("priority", "name"),
        fix_versions: f.get("fixVersions").and_then(Value::as_array)
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

#[cfg(test)]
mod tests {
    use super::*;

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
