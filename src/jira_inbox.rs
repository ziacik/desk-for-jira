//! Remote Jira activity for issues assigned now, previously assigned, or edited locally.
//! Unlike activity history, these events come from Jira and are authored by other users.
use std::{collections::HashSet, time::{Duration, SystemTime, UNIX_EPOCH}};

use anyhow::{Context, Result, bail};
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::jira::{AuthMode, Credentials};

const MAX_ISSUES: usize = 60;
const MAX_ENTRIES: usize = 300;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub key: String,
    pub summary: String,
    pub action: String,
    pub author: String,
    pub created: String,
    #[serde(default)]
    pub priority: bool,
    #[serde(default)]
    pub read: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Feed {
    pub entries: Vec<Entry>,
    pub last_polled: i64,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .unwrap_or_default().as_secs() as i64
}

pub fn lookback_minutes(feed: &Feed) -> u32 {
    if feed.last_polled == 0 {
        return 24 * 60; // Bootstrap with recent activity, rather than every historical change.
    }
    ((now().saturating_sub(feed.last_polled) / 60) + 5).clamp(5, 7 * 24 * 60) as u32
}

impl Feed {
    pub fn unread(&self) -> usize {
        self.entries.iter().filter(|entry| !entry.read).count()
    }

    pub fn merge(&mut self, mut fetched: Vec<Entry>) {
        let known: HashSet<String> = self.entries.iter().map(|e| e.id.clone()).collect();
        fetched.retain(|entry| !known.contains(&entry.id));
        self.entries.extend(fetched);
        self.entries.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
        self.entries.truncate(MAX_ENTRIES);
        self.last_polled = now();
    }

    pub fn read(&mut self, id: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.read = true;
        }
    }

    pub fn read_all(&mut self) {
        for entry in &mut self.entries { entry.read = true; }
    }
}

fn authorized(request: RequestBuilder, account: &Credentials) -> RequestBuilder {
    match account.mode {
        AuthMode::Cloud => request.basic_auth(&account.email, Some(&account.token)),
        AuthMode::Server => request.bearer_auth(&account.token),
    }
}

fn json_response(request: RequestBuilder, account: &Credentials) -> Result<Value> {
    let response = authorized(request, account)
        .header(reqwest::header::ACCEPT, "application/json")
        .send().context("Could not contact Jira for activity")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Jira activity request failed: HTTP {}", status.as_u16());
    }
    response.json().context("Invalid Jira activity response")
}

fn user_id(user: &Value) -> Option<&str> {
    ["accountId", "key", "name"].iter()
        .find_map(|field| user.get(field).and_then(Value::as_str).filter(|id| !id.is_empty()))
}

fn same_user(user: &Value, me: &Value) -> bool {
    ["accountId", "key", "name"].iter().any(|field| {
        user.get(field).and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .is_some_and(|id| me.get(field).and_then(Value::as_str) == Some(id))
    })
}

fn assigned_to_me(item: &Value, me: &Value) -> bool {
    let to = item.get("to").and_then(Value::as_str);
    let ids = ["accountId", "key", "name"];
    ids.iter().any(|field| to.is_some() && to == me.get(field).and_then(Value::as_str))
}

/// Whitelist issue keys before inserting the locally tracked set into JQL.
fn valid_key(key: &str) -> bool {
    let Some((project, number)) = key.split_once('-') else { return false; };
    !project.is_empty() && project.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
}

fn jql(minutes: u32, watched: &[String], include_history: bool) -> String {
    let mut conditions = vec!["assignee = currentUser()".to_string()];
    if include_history {
        conditions.push("assignee WAS currentUser()".to_string());
    }
    let keys: Vec<&str> = watched.iter().map(String::as_str).filter(|key| valid_key(key)).take(30).collect();
    if !keys.is_empty() {
        conditions.push(format!("key in ({})", keys.join(", ")));
    }
    format!("({}) AND updated >= -{}m ORDER BY updated DESC", conditions.join(" OR "), minutes)
}

fn search_issues(client: &Client, account: &Credentials, minutes: u32, watched: &[String]) -> Result<Vec<Value>> {
    let path = match account.mode {
        AuthMode::Cloud => "/rest/api/3/search/jql",
        AuthMode::Server => "/rest/api/2/search",
    };
    let url = format!("{}{}", account.base_url, path);
    let search = |include_history: bool| {
        json_response(client.post(&url).json(&json!({
            "jql": jql(minutes, watched, include_history),
            "maxResults": MAX_ISSUES,
            "fields": ["summary", "issuetype", "assignee", "status"]
        })), account)
    };
    // Some older Jira installations do not support history searches for assignee.
    let data = search(true).or_else(|_| search(false))?;
    Ok(data.get("issues").and_then(Value::as_array).cloned().unwrap_or_default())
}

fn last_page(client: &Client, account: &Credentials, path: &str, array_key: &str) -> Result<Vec<Value>> {
    let url = format!("{}{}", account.base_url, path);
    let first = json_response(client.get(&url).query(&[("startAt", 0), ("maxResults", 100)]), account)?;
    let values = first.get(array_key).and_then(Value::as_array)
        .context("Jira activity page is missing its entries")?;
    let total = first.get("total").and_then(Value::as_u64).unwrap_or(values.len() as u64);
    if total <= values.len() as u64 {
        return Ok(values.clone());
    }
    let start_at = total.saturating_sub(100);
    let last = json_response(client.get(&url)
        .query(&[("startAt", start_at), ("maxResults", 100)]), account)?;
    Ok(last.get(array_key).and_then(Value::as_array).cloned().unwrap_or_default())
}

fn changelogs(client: &Client, account: &Credentials, key: &str) -> Result<Vec<Value>> {
    if account.mode == AuthMode::Cloud {
        return last_page(client, account,
            &format!("/rest/api/3/issue/{key}/changelog"), "values");
    }
    // Server/Data Center's REST v2 does not consistently implement /changelog.
    // The supported fallback is the expanded issue changelog.
    let issue = json_response(client.get(format!("{}/rest/api/2/issue/{key}", account.base_url))
        .query(&[("expand", "changelog"), ("fields", "summary")]), account)?;
    Ok(issue.pointer("/changelog/histories").and_then(Value::as_array)
        .cloned().unwrap_or_default())
}

fn parse_events(key: &str, summary: &str, issue: &Value, histories: &[Value], comments: &[Value], me: &Value) -> Vec<Entry> {
    let is_bug = issue.pointer("/fields/issuetype/name").and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("bug"));
    let current_todo = issue.pointer("/fields/status/statusCategory/key").and_then(Value::as_str) == Some("new");
    let assignee_is_me = issue.pointer("/fields/assignee").is_some_and(|user| same_user(user, me));
    let mut events = Vec::new();
    for history in histories {
        let Some(author) = history.get("author") else { continue; };
        if same_user(author, me) { continue; }
        let author_name = author.get("displayName").and_then(Value::as_str).unwrap_or("Someone");
        let id = history.get("id").and_then(Value::as_str).unwrap_or_default();
        let created = history.get("created").and_then(Value::as_str).unwrap_or_default();
        for (index, item) in history.get("items").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let field = item.get("field").and_then(Value::as_str).unwrap_or_default();
            let (action, priority) = match field.to_ascii_lowercase().as_str() {
                "status" => {
                    let from = item.get("fromString").and_then(Value::as_str).unwrap_or("?");
                    let to = item.get("toString").and_then(Value::as_str).unwrap_or("?");
                    if from == to { continue; }
                    let returned = is_bug && current_todo && assignee_is_me;
                    (format!("{}: {from} → {to}", if returned { "Bug returned" } else { "Status changed" }), returned)
                }
                "assignee" if assigned_to_me(item, me) => ("Assigned to you".to_string(), true),
                _ => continue,
            };
            events.push(Entry {
                id: format!("change:{key}:{id}:{index}"), key: key.into(), summary: summary.into(),
                action, author: author_name.into(), created: created.into(),
                priority, read: false,
            });
        }
    }
    for comment in comments {
        let Some(author) = comment.get("author") else { continue; };
        if same_user(author, me) { continue; }
        let Some(id) = comment.get("id").and_then(Value::as_str) else { continue; };
        events.push(Entry {
            id: format!("comment:{key}:{id}"), key: key.into(), summary: summary.into(),
            action: "New comment".into(),
            author: author.get("displayName").and_then(Value::as_str).unwrap_or("Someone").into(),
            created: comment.get("created").and_then(Value::as_str).unwrap_or_default().into(),
            priority: false, read: false,
        });
    }
    events
}

pub fn fetch(account: Credentials, minutes: u32, watched: &[String]) -> Result<Vec<Entry>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(25))
        .user_agent("DeskForJira/0.1")
        .build()?;
    let version = if account.mode == AuthMode::Cloud { "3" } else { "2" };
    let me = json_response(client.get(format!("{}/rest/api/{version}/myself", account.base_url)), &account)?;
    // Require a stable identity: comparing display names could hide colleagues' activity.
    if user_id(&me).is_none() {
        bail!("Jira did not return a user identity");
    }
    let issues = search_issues(&client, &account, minutes, watched)?;
    let mut events = Vec::new();
    for issue in issues {
        let Some(key) = issue.get("key").and_then(Value::as_str).filter(|key| valid_key(key)) else { continue; };
        let summary = issue.pointer("/fields/summary").and_then(Value::as_str).unwrap_or("");
        // Failures on an individual issue should not hide all other activity.
        let histories = changelogs(&client, &account, key).unwrap_or_default();
        let comments = last_page(&client, &account,
            &format!("/rest/api/{version}/issue/{key}/comment"), "comments").unwrap_or_default();
        events.extend(parse_events(key, summary, &issue, &histories, &comments, &me));
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> Entry {
        Entry { id: id.into(), key: "ART-12".into(), summary: "Bug".into(),
            action: "New comment".into(), author: "Tester".into(), created: id.into(),
            priority: false, read: false }
    }

    #[test]
    fn deduplicates_and_preserves_read_status() {
        let mut feed = Feed::default();
        feed.merge(vec![sample("1"), sample("2")]);
        feed.read("2");
        feed.merge(vec![sample("2"), sample("3")]);
        assert_eq!(feed.entries.len(), 3);
        assert!(feed.entries.iter().find(|entry| entry.id == "2").unwrap().read);
        assert_eq!(feed.unread(), 2);
    }

    #[test]
    fn jql_does_not_interpolate_untrusted_keys() {
        let query = jql(10, &["ART-12".into(), "ART-1) OR 1=1".into()], true);
        assert!(query.contains("key in (ART-12)"));
        assert!(!query.contains("1=1"));
    }

    #[test]
    fn ignores_own_changes_and_detects_returned_bugs() {
        let me = json!({"accountId": "me"});
        let issue = json!({"fields": {
            "issuetype": {"name": "Bug"},
            "status": {"statusCategory": {"key": "new"}},
            "assignee": {"accountId": "me"}
        }});
        let history = json!([
            {"id":"10","author":{"accountId":"me"},"items":[{"field":"status","fromString":"Testing","toString":"Ready to dev"}]},
            {"id":"11","author":{"accountId":"tester","displayName":"Tester"},"items":[{"field":"status","fromString":"Testing","toString":"Ready to dev"}]}
        ]);
        let entries = parse_events("ART-12", "Bug", &issue, history.as_array().unwrap(), &[], &me);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].priority);
        assert!(entries[0].action.starts_with("Bug returned"));
    }
}
