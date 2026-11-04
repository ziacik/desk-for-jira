use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PAGE_SIZE: usize = 100;
const MAX_ITEMS: usize = 500;
const FIELDS: [&str; 9] = [
    "summary",
    "status",
    "assignee",
    "issuetype",
    "priority",
    "updated",
    "description",
    "reporter",
    "project",
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

#[derive(Clone, Debug)]
pub struct Issue {
    pub key: String,
    pub project_key: String,
    pub summary: String,
    pub status: String,
    pub status_id: String,
    pub kind: String,
    pub priority: String,
    pub assignee: String,
    pub updated: String,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub key: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct ProjectStatus {
    pub project_key: String,
    pub id: String,
    pub name: String,
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
        let mut result = Vec::new();
        let mut next_token: Option<String> = None;
        let mut start_at = 0usize;

        loop {
            let data = match self.account.mode {
                AuthMode::Cloud => {
                    let mut payload = json!({
                        "jql": jql,
                        "maxResults": PAGE_SIZE,
                        "fields": FIELDS,
                    });
                    if let Some(ref token) = next_token {
                        payload["nextPageToken"] = json!(token);
                    }
                    self.read_json(self.http.post(self.url("/rest/api/3/search/jql")).json(&payload))?
                }
                AuthMode::Server => self.read_json(
                    self.http.post(self.url("/rest/api/2/search"))
                        .json(&json!({"jql":jql, "maxResults":PAGE_SIZE, "startAt":start_at, "fields":FIELDS}))
                )?,
            };
            let page = parse_issues(&data);
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
                            project_key: key.clone(), id: id.to_owned(), name: name.to_owned(),
                        });
                    }
                }
            }
        }
        Ok(statuses)
    }

    pub fn project_issues(&self, keys: &[String], scope: Option<&str>) -> Result<Vec<Issue>> {
        if keys.is_empty() { return Ok(Vec::new()); }
        for key in keys { validate_project_key(key)?; }
        let projects = keys.join(", ");
        let mut jql = format!("project IN ({projects})");
        if let Some(scope) = scope {
            jql.push_str(" AND (");
            jql.push_str(scope);
            jql.push(')');
        }
        jql.push_str(" ORDER BY updated DESC");
        self.search_issues(&jql)
    }

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
    data.get("issues").and_then(Value::as_array)
        .into_iter().flatten().filter_map(parse_issue).collect()
}

fn parse_issue(v: &Value) -> Option<Issue> {
    let key = v.get("key")?.as_str()?.to_owned();
    let f = v.get("fields")?;
    let field = |name: &str| f.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    let nested = |key: &str, name: &str| f.get(key).and_then(|v| v.get(name))
        .and_then(Value::as_str).unwrap_or("").to_owned();
    Some(Issue {
        project_key: f.get("project").and_then(|p| p.get("key"))
            .and_then(Value::as_str).map(str::to_owned)
            .unwrap_or_else(|| key.split('-').next().unwrap_or("").to_owned()),
        key,
        summary: field("summary"),
        status: nested("status", "name"),
        status_id: nested("status", "id"),
        kind: nested("issuetype", "name"),
        priority: nested("priority", "name"),
        assignee: nested("assignee", "displayName"),
        updated: field("updated").chars().take(10).collect(),
        description: format_description(f.get("description")),
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
    }
}
