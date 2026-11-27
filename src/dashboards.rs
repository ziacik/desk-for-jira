//! Read-only dashboard import and native release / filter data.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::jira::{Issue, JiraClient};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dashboard {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub project: String,
}

#[derive(Debug)]
pub struct Gadget {
    pub title: String,
    pub data: Result<Vec<Issue>, String>,
}

#[derive(Debug)]
pub struct Release {
    pub id: String,
    pub name: String,
    pub date: String,
}

fn id(value: &Value) -> Option<String> {
    let value = value.as_str().map(str::to_owned).or_else(|| value.as_u64().map(|n| n.to_string()))?;
    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())).then_some(value)
}

fn dashboard(value: &Value) -> Option<Dashboard> {
    Some(Dashboard {
        id: id(value.get("id")?)?,
        name: value.get("name")?.as_str()?.to_owned(),
        project: String::new(),
    })
}

pub fn list(client: &JiraClient) -> Result<Vec<Dashboard>> {
    let mut result = Vec::new();
    let mut start = 0;
    loop {
        let page = client.dashboard_get("/rest/api/3/dashboard/search", &[("startAt", start), ("maxResults", 100)])?;
        let values = page["values"].as_array().context("Invalid dashboard search response")?;
        result.extend(values.iter().filter_map(dashboard));
        start += values.len();
        if values.is_empty() || page["isLast"].as_bool() == Some(true)
            || page["total"].as_u64().is_some_and(|total| start as u64 >= total) {
            break;
        }
    }
    result.sort_by_key(|d| d.name.to_lowercase());
    Ok(result)
}

pub fn parse_dashboard_id(input: &str, site: &str) -> Result<String> {
    let input = input.trim();
    if let Some(id) = id(&Value::String(input.to_owned())) { return Ok(id); }
    let url = reqwest::Url::parse(input).context("Enter a dashboard URL or numeric ID")?;
    let base = reqwest::Url::parse(site)?;
    ensure!(url.origin() == base.origin(), "The dashboard must belong to the connected Jira site");
    let value = url.path().trim_end_matches('/').rsplit_once("/dashboards/")
        .map(|(_, value)| value.to_owned())
        .or_else(|| url.query_pairs().find(|(key, _)| key == "selectPageId").map(|(_, value)| value.into_owned()))
        .context("Not a Jira dashboard URL")?;
    id(&Value::String(value)).context("Invalid dashboard ID")
}

pub fn by_id(client: &JiraClient, dashboard_id: &str) -> Result<Dashboard> {
    ensure!(id(&Value::String(dashboard_id.to_owned())).is_some(), "Invalid dashboard ID");
    dashboard(&client.dashboard_get(&format!("/rest/api/3/dashboard/{dashboard_id}"), &[])?)
        .context("Invalid dashboard response")
}

// Only recognize explicit saved-filter identifiers; never treat project IDs
// or arbitrary numbers in opaque gadget configuration as filter IDs.
fn filter_id(value: &Value) -> Option<String> {
    fn collect(value: &Value, ids: &mut std::collections::HashSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let candidate = match key.as_str() {
                        "filterId" | "savedFilterId" | "filter-id" | "filter_id" => id(value),
                        "filter" | "projectOrFilterId" => value.as_str()
                            .and_then(|s| s.strip_prefix("filter-"))
                            .and_then(|s| id(&Value::String(s.to_owned()))),
                        _ => None,
                    };
                    if let Some(candidate) = candidate { ids.insert(candidate); }
                    collect(value, ids);
                }
            }
            Value::Array(values) => for value in values { collect(value, ids); },
            _ => {}
        }
    }
    let mut ids = std::collections::HashSet::new();
    collect(value, &mut ids);
    if ids.len() == 1 { ids.into_iter().next() } else { None }
}

fn gadget_filter(client: &JiraClient, dashboard_id: &str, gadget: &Value) -> Result<Vec<Issue>> {
    let item = id(&gadget["id"]).context("Missing gadget ID")?;
    let path = format!("/rest/api/3/dashboard/{dashboard_id}/items/{item}/properties");
    let keys = client.dashboard_get(&path, &[])?;
    let mut configs = vec![gadget.clone()];
    for key in keys["keys"].as_array().into_iter().flatten().filter_map(|key| key["key"].as_str()) {
        // Encode the property as one path segment rather than accepting a URL
        // from the server or interpolating an untrusted path.
        let mut url = reqwest::Url::parse("https://jira.invalid/")?;
        url.path_segments_mut().map_err(|_| anyhow::anyhow!("Invalid property URL"))?.push(key);
        let encoded = url.path().trim_start_matches('/');
        let property = client.dashboard_get(&format!("{path}/{encoded}"), &[])?;
        configs.push(property["value"].clone());
    }
    let filter = filter_id(&Value::Array(configs)).context("Jira did not expose a unique saved filter for this gadget")?;
    client.search_issues(&format!("filter = {filter}"))
}

pub fn gadgets(client: &JiraClient, dashboard_id: &str) -> Result<Vec<Gadget>> {
    ensure!(id(&Value::String(dashboard_id.to_owned())).is_some(), "Invalid dashboard ID");
    let response = client.dashboard_get(&format!("/rest/api/3/dashboard/{dashboard_id}/gadget"), &[])?;
    let mut values = response["gadgets"].as_array().context("Invalid dashboard gadget response")?.clone();
    values.sort_by_key(|g| (g["position"]["column"].as_u64(), g["position"]["row"].as_u64()));
    Ok(values.iter().map(|g| {
        let title = g["title"].as_str().unwrap_or("Dashboard gadget").to_owned();
        let kind = format!("{} {} {}", title, g["moduleKey"], g["uri"]).to_lowercase();
        let data = if kind.contains("filter-results") || kind.contains("filterresults") || kind.contains("filter results") {
            gadget_filter(client, dashboard_id, g).map_err(|e| e.to_string())
        } else {
            Err("This gadget type is not yet supported natively. Open the original dashboard to view it.".to_owned())
        };
        Gadget { title, data }
    }).collect())
}

pub fn releases(client: &JiraClient, project: &str) -> Result<Vec<Release>> {
    ensure!(!project.is_empty() && project.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'), "Invalid project key");
    let response = client.dashboard_get(&format!("/rest/api/3/project/{project}/versions"), &[])?;
    let mut versions: Vec<_> = response.as_array().context("Invalid project versions response")?.iter()
        .filter(|v| v["released"].as_bool() == Some(false) && v["archived"].as_bool() != Some(true))
        .filter_map(|v| Some(Release {
            id: id(&v["id"])?,
            name: v["name"].as_str()?.to_owned(),
            date: v["releaseDate"].as_str().unwrap_or("").to_owned(),
        })).collect();
    versions.sort_by(|a, b| (a.date.is_empty(), &a.date, &a.name).cmp(&(b.date.is_empty(), &b.date, &b.name)));
    Ok(versions)
}

pub fn release_bugs(client: &JiraClient, version: &str) -> Result<Vec<Issue>> {
    ensure!(id(&Value::String(version.to_owned())).is_some(), "Invalid release ID");
    client.search_issues(&format!("issuetype = Bug AND fixVersion = {version} ORDER BY priority DESC, key ASC"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dashboard_urls_stay_on_connected_site() {
        let site = "https://example.atlassian.net";
        assert_eq!(parse_dashboard_id("https://example.atlassian.net/jira/dashboards/10037", site).unwrap(), "10037");
        assert_eq!(parse_dashboard_id("https://example.atlassian.net/Dashboard.jspa?selectPageId=42", site).unwrap(), "42");
        assert!(parse_dashboard_id("https://other.atlassian.net/jira/dashboards/42", site).is_err());
        assert!(parse_dashboard_id("https://example.atlassian.net/jira/dashboards/../42", site).is_err());
    }

    #[test]
    fn filter_ids_must_be_explicit_and_unambiguous() {
        assert_eq!(filter_id(&json!({"prefs":{"filterId":"12"}})).as_deref(), Some("12"));
        assert_eq!(filter_id(&json!({"projectOrFilterId":"filter-23"})).as_deref(), Some("23"));
        assert_eq!(filter_id(&json!({"projectId":12})), None);
        assert_eq!(filter_id(&json!([{"filterId":12},{"filterId":13}])), None);
    }
}
