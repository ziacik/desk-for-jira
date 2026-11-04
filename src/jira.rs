use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PAGE_SIZE: usize = 100;
const MAX_ITEMS: usize = 500;
const FIELDS: [&str; 8] = [
    "summary",
    "status",
    "assignee",
    "issuetype",
    "priority",
    "updated",
    "description",
    "reporter",
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
pub struct Board {
    pub id: u64,
    pub name: String,
    pub board_type: String,
}

#[derive(Clone, Debug)]
pub struct BoardColumn {
    pub name: String,
    pub status_ids: Vec<String>,
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

    pub fn my_issues(&self) -> Result<Vec<Issue>> {
        self.search_issues("assignee = currentUser() ORDER BY updated DESC")
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

    pub fn boards(&self) -> Result<Vec<Board>> {
        let mut boards = Vec::new();
        let mut start = 0usize;
        loop {
            let data = self.read_json(
                self.http.get(self.url("/rest/agile/1.0/board"))
                    .query(&[("startAt", start), ("maxResults", PAGE_SIZE)])
            )?;
            let page: Vec<Board> = data.get("values").and_then(Value::as_array)
                .into_iter().flatten().filter_map(|v| {
                    Some(Board {
                        id: v.get("id")?.as_u64()?,
                        name: v.get("name")?.as_str()?.to_owned(),
                        board_type: v.get("type").and_then(Value::as_str).unwrap_or("board").to_owned(),
                    })
                }).collect();
            let count = page.len();
            boards.extend(page);
            start += count;
            if count == 0 || boards.len() >= MAX_ITEMS
                || data.get("isLast").and_then(Value::as_bool) == Some(true) {
                break;
            }
        }
        boards.truncate(MAX_ITEMS);
        Ok(boards)
    }

    pub fn board_issues(&self, board_id: u64) -> Result<Vec<Issue>> {
        self.fetch_board_issues(board_id, "issue")
    }

    pub fn board_backlog(&self, board_id: u64) -> Result<Vec<Issue>> {
        self.fetch_board_issues(board_id, "backlog")
    }

    fn fetch_board_issues(&self, board_id: u64, section: &str) -> Result<Vec<Issue>> {
        let mut issues = Vec::new();
        let mut next_token: Option<String> = None;
        let mut start = 0usize;
        loop {
            let path = if self.account.mode == AuthMode::Cloud {
                format!("/rest/software/1.0/board/{board_id}/{section}")
            } else {
                format!("/rest/agile/1.0/board/{board_id}/{section}")
            };
            let mut request = self.http.get(self.url(&path))
                .query(&[("maxResults", PAGE_SIZE.to_string()),
                          ("fields", FIELDS.join(","))]);
            if let Some(ref token) = next_token {
                request = request.query(&[("nextPageToken", token)]);
            } else if self.account.mode == AuthMode::Server {
                request = request.query(&[("startAt", start.to_string())]);
            }
            let data = self.read_json(request)?;
            let page = parse_issues(&data);
            let count = page.len();
            issues.extend(page);
            if count == 0 || issues.len() >= MAX_ITEMS {
                break;
            }
            if self.account.mode == AuthMode::Cloud {
                next_token = data.get("nextPageToken").and_then(Value::as_str).map(str::to_owned);
                if next_token.is_none() || data.get("isLast").and_then(Value::as_bool) == Some(true) {
                    break;
                }
            } else {
                start += count;
                if start >= data.get("total").and_then(Value::as_u64).unwrap_or(start as u64) as usize {
                    break;
                }
            }
        }
        issues.truncate(MAX_ITEMS);
        Ok(issues)
    }

    pub fn board_columns(&self, board_id: u64) -> Result<Vec<BoardColumn>> {
        let data = self.read_json(self.http.get(
            self.url(&format!("/rest/agile/1.0/board/{board_id}/configuration"))
        ))?;
        let cols = data.pointer("/columnConfig/columns").and_then(Value::as_array);
        Ok(cols.into_iter().flatten().map(|v| BoardColumn {
            name: v.get("name").and_then(Value::as_str).unwrap_or("Column").to_owned(),
            status_ids: v.get("statuses").and_then(Value::as_array)
                .into_iter().flatten()
                .filter_map(|s| s.get("id").and_then(Value::as_str).map(str::to_owned)).collect(),
        }).collect())
    }

    pub fn browse_issue_url(&self, key: &str) -> String {
        format!("{}/browse/{}", self.account.base_url, key)
    }
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
