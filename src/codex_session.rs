//! JSON-RPC transport for Codex app-server. All subprocess I/O stays off the GTK thread.
use std::{
    collections::HashSet,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{ChildStdin, Command as ProcessCommand, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{codex::{self, CodexMode}, git, jira::{Credentials, Issue, JiraClient}, preferences};

pub enum Control {
    Send(String),
    Interrupt,
    Decide { request_id: Value, allow: bool },
    Quit,
}

pub enum Event {
    Status(String),
    Output(String),
    Ready,
    Busy(bool),
    Approval { request_id: Value, description: String },
    Error(String),
    Closed,
}

pub fn spawn(
    worktree: PathBuf, site: String, issue: Issue, account: Option<Credentials>,
) -> (Sender<Control>, Receiver<Event>) {
    let (commands, command_rx) = mpsc::channel();
    let (events, event_rx) = mpsc::channel();
    thread::spawn(move || {
        if let Err(error) = run(worktree, site, issue, account, command_rx, &events) {
            let _ = events.send(Event::Error(error.to_string()));
        }
        let _ = events.send(Event::Closed);
    });
    (commands, event_rx)
}

fn send(stdin: &mut ChildStdin, value: Value) -> Result<()> {
    serde_json::to_writer(&mut *stdin, &value)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn input(text: String) -> Value {
    json!([{ "type": "text", "text": text }])
}

fn item_description(item: &Value) -> Option<String> {
    match item["type"].as_str()? {
        "commandExecution" => {
            let command = item["command"].as_str().unwrap_or("(unknown command)");
            Some(format!("\n$ {command}\n"))
        }
        "fileChange" => {
            let files = item["changes"].as_array()
                .map(|changes| changes.iter().filter_map(|change| change["path"].as_str())
                    .collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            Some(format!("\nFiles changed: {files}\n"))
        }
        "webSearch" => Some("\nWeb search\n".to_owned()),
        _ => None,
    }
}

fn message_text(item: &Value) -> Option<String> {
    if let Some(text) = item["text"].as_str() {
        return Some(text.to_owned());
    }
    let content = item["content"].as_array()?;
    let parts: Vec<_> = content.iter().filter_map(|part| part["text"].as_str()).collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn historical_items(response: &Value) -> Vec<String> {
    let mut result = Vec::new();
    if let Some(turns) = response["result"]["data"].as_array() {
        for entry in turns.iter().rev() {
            let turn = entry.get("turn").unwrap_or(entry);
            if let Some(items) = turn["items"].as_array() {
                for item in items {
                    match item["type"].as_str() {
                        Some("userMessage") | Some("agentMessage") => {
                            if let Some(text) = message_text(item) {
                                let role = if item["type"] == "userMessage" { "You" } else { "Codex" };
                                result.push(format!("\n{role}: {text}\n"));
                            }
                        }
                        Some("commandExecution") => {
                            if let Some(line) = item_description(item) { result.push(line); }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    result
}

struct Session<'a> {
    stdin: ChildStdin,
    events: &'a Sender<Event>,
    site: String,
    issue_key: String,
    worktree: PathBuf,
    thread_id: Option<String>,
    turn_id: Option<String>,
    initial_prompt: Option<String>,
    auto_start: bool,
    resuming: bool,
    yolo: bool,
    next_request: u64,
    streamed_messages: HashSet<String>,
}

impl Session<'_> {
    fn request(&mut self, method: &str, params: Value) -> Result<()> {
        let id = self.next_request;
        self.next_request += 1;
        send(&mut self.stdin, json!({"id": id, "method": method, "params": params}))
    }

    fn begin_turn(&mut self, text: String) -> Result<()> {
        let Some(thread_id) = self.thread_id.clone() else { return Ok(()) };
        let text = if let Some(context) = self.initial_prompt.take() {
            format!("{context}\n\nUser's next instruction:\n{text}")
        } else {
            text
        };
        self.request("turn/start", json!({
            "threadId": thread_id,
            "input": input(text),
            "cwd": self.worktree,
            "approvalPolicy": if self.yolo { "never" } else { "on-request" },
            "sandboxPolicy": {"type": if self.yolo { "dangerFullAccess" } else { "workspaceWrite" }},
        }))?;
        let _ = self.events.send(Event::Busy(true));
        Ok(())
    }

    fn handle(&mut self, value: &Value) -> Result<()> {
        if let Some(request_id) = value.get("id") {
            // Server initiated approval request: preserve its ID exactly in the response.
            if let Some(method) = value["method"].as_str() {
                if matches!(method, "item/commandExecution/requestApproval" | "item/fileChange/requestApproval") {
                    let params = &value["params"];
                    let detail = params["reason"].as_str()
                        .or_else(|| params["command"].as_str())
                        .unwrap_or(if method.contains("fileChange") {
                            "Codex wants to edit files outside the current permissions."
                        } else {
                            "Codex requests permission to run a command."
                        });
                    let _ = self.events.send(Event::Approval {
                        request_id: request_id.clone(), description: detail.to_owned(),
                    });
                } else {
                    send(&mut self.stdin, json!({"id": request_id, "error": {
                        "code": -32601, "message": "Unsupported client request"
                    }}))?;
                }
                return Ok(());
            }
            if let Some(error) = value.get("error") {
                let message = error["message"].as_str().unwrap_or("Unknown Codex error");
                if request_id == 2 && self.thread_id.is_none() {
                    // An old thread may have been deleted externally; recover with a new one.
                    let _ = self.events.send(Event::Status(format!(
                        "Cannot resume previous conversation ({message}); creating a new one…"
                    )));
                    self.resuming = false;
                    self.request("thread/start", json!({
                        "cwd": self.worktree,
                        "approvalPolicy": if self.yolo { "never" } else { "on-request" },
                        "sandbox": if self.yolo { "danger-full-access" } else { "workspace-write" },
                    }))?;
                } else {
                    let _ = self.events.send(Event::Error(message.to_owned()));
                    let _ = self.events.send(Event::Busy(false));
                }
                return Ok(());
            }
            if request_id == 1 {
                send(&mut self.stdin, json!({"method": "initialized", "params": {}}))?;
                if let Some(thread_id) = preferences::codex_thread(
                    &self.site, &self.issue_key, &self.worktree.to_string_lossy(),
                )? {
                    self.resuming = true;
                    self.request("thread/resume", json!({
                        "threadId": thread_id, "excludeTurns": true
                    }))?;
                } else {
                    self.request("thread/start", json!({
                        "cwd": self.worktree,
                        "approvalPolicy": if self.yolo { "never" } else { "on-request" },
                        "sandbox": if self.yolo { "danger-full-access" } else { "workspace-write" },
                    }))?;
                }
            } else if let Some(id) = value["result"]["thread"]["id"].as_str() {
                // Resuming an existing conversation must NEVER replay the ticket prompt.
                if self.resuming {
                    self.resuming = false;
                    self.auto_start = false;
                    self.initial_prompt = None;
                }
                // Exactly one logical thread belongs to a site/ticket/worktree.
                self.thread_id = Some(id.to_owned());
                preferences::remember_codex_thread(
                    &self.site, &self.issue_key, &self.worktree.to_string_lossy(), id,
                )?;
                let _ = self.events.send(Event::Ready);
                if self.auto_start {
                    self.auto_start = false;
                    if let Some(prompt) = self.initial_prompt.take() {
                        self.begin_turn(prompt)?;
                    }
                } else {
                    self.request("thread/turns/list", json!({
                        "threadId": id, "limit": 30, "sortDirection": "desc", "itemsView": "full"
                    }))?;
                }
            } else if let Some(id) = value["result"]["turn"]["id"].as_str() {
                self.turn_id = Some(id.to_owned());
            } else {
                for message in historical_items(value) {
                    let _ = self.events.send(Event::Output(message));
                }
            }
            return Ok(());
        }

        match value["method"].as_str().unwrap_or("") {
            "turn/started" => {
                if let Some(id) = value["params"]["turn"]["id"].as_str() {
                    self.turn_id = Some(id.to_owned());
                }
                let _ = self.events.send(Event::Busy(true));
            }
            "turn/completed" => {
                self.turn_id = None;
                let status = value["params"]["turn"]["status"].as_str().unwrap_or("finished");
                let _ = self.events.send(Event::Status(format!("Codex: {status}")));
                let _ = self.events.send(Event::Busy(false));
            }
            "item/started" => {
                if let Some(line) = item_description(&value["params"]["item"]) {
                    let _ = self.events.send(Event::Output(line));
                }
            }
            "item/agentMessage/delta" => {
                let params = &value["params"];
                if let Some(id) = params["itemId"].as_str() {
                    self.streamed_messages.insert(id.to_owned());
                }
                if let Some(delta) = params["delta"].as_str() {
                    let _ = self.events.send(Event::Output(delta.to_owned()));
                }
            }
            "item/commandExecution/outputDelta" => {
                if let Some(delta) = value["params"]["delta"].as_str() {
                    let _ = self.events.send(Event::Output(delta.to_owned()));
                }
            }
            "item/completed" => {
                let item = &value["params"]["item"];
                if item["type"] == "agentMessage" {
                    let streamed = item["id"].as_str()
                        .is_some_and(|id| self.streamed_messages.remove(id));
                    if !streamed {
                        if let Some(text) = message_text(item) {
                            let _ = self.events.send(Event::Output(text));
                        }
                    }
                    let _ = self.events.send(Event::Output("\n\n".to_owned()));
                }
            }
            "error" => {
                let message = value["params"]["error"]["message"].as_str()
                    .unwrap_or("Codex reported an error");
                let _ = self.events.send(Event::Error(message.to_owned()));
            }
            _ => {}
        }
        Ok(())
    }
}

fn run(
    worktree: PathBuf, site: String, issue: Issue, account: Option<Credentials>,
    commands: Receiver<Control>, events: &Sender<Event>,
) -> Result<()> {
    if !git::is_repository(&worktree) {
        bail!("Missing Git worktree: {}", worktree.display());
    }
    let settings = preferences::load_git()?;
    let status = git::status(&worktree).ok().map(|items| !items.is_empty());
    let mode = CodexMode::for_issue(&issue).for_changes(status);
    let _ = events.send(Event::Status("Loading Jira comments…".to_owned()));
    let comments = account
        .and_then(|credentials| JiraClient::new(credentials).ok())
        .and_then(|client| client.issue_comments(&issue.key).ok())
        .unwrap_or_default();
    let prompt = codex::with_custom_instructions(
        codex::issue_prompt(&site, &issue, &comments, mode),
        &settings.codex_instructions,
    );
    let initial = if mode == CodexMode::Open { Some(prompt) } else { None };
    let automatic = mode != CodexMode::Open;
    let mut child = ProcessCommand::new("codex")
        .arg("app-server")
        .current_dir(&worktree)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().context("Cannot start Codex app-server (is codex in PATH?)")?;
    let stdin = child.stdin.take().context("Codex stdin unavailable")?;
    let stdout = child.stdout.take().context("Codex stdout unavailable")?;
    let (lines_tx, lines_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines_tx.send(line).is_err() { break; }
        }
    });
    let mut session = Session {
        stdin, events, site, issue_key: issue.key, worktree,
        thread_id: None, turn_id: None,
        initial_prompt: Some(if automatic { prompt } else { initial.unwrap_or_default() }),
        auto_start: automatic, resuming: false, yolo: settings.codex_yolo, next_request: 2,
        streamed_messages: HashSet::new(),
    };
    send(&mut session.stdin, json!({
        "id": 1, "method": "initialize",
        "params": {"clientInfo": {
            "name": "desk_for_jira", "title": "Desk for Jira", "version": "0.1.0"
        }}
    }))?;
    let _ = events.send(Event::Status("Connecting to Codex…".to_owned()));
    loop {
        match commands.try_recv() {
            Ok(Control::Quit) => break,
            Ok(Control::Send(message)) => {
                if !message.trim().is_empty() {
                    session.begin_turn(message)?;
                }
            }
            Ok(Control::Interrupt) => {
                if let (Some(thread_id), Some(turn_id)) =
                    (session.thread_id.clone(), session.turn_id.clone())
                {
                    session.request("turn/interrupt", json!({
                        "threadId": thread_id, "turnId": turn_id
                    }))?;
                }
            }
            Ok(Control::Decide { request_id, allow }) => {
                send(&mut session.stdin, json!({
                    "id": request_id,
                    "result": {"decision": if allow { "accept" } else { "decline" }}
                }))?;
            }
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }
        match lines_rx.recv_timeout(Duration::from_millis(60)) {
            Ok(Ok(line)) => {
                match serde_json::from_str::<Value>(&line) {
                    Ok(value) => session.handle(&value)?,
                    Err(error) => {
                        let _ = events.send(Event::Error(format!("Invalid Codex response: {error}")));
                    }
                }
            }
            Ok(Err(error)) => bail!("Reading Codex output failed: {error}"),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("Codex app-server exited unexpectedly");
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_history_contains_text_and_commands_without_diffs() {
        let data = json!({"result":{"data":[{"items":[
            {"type":"userMessage","content":[{"type":"text","text":"hello"}]},
            {"type":"commandExecution","command":"cargo test"},
            {"type":"agentMessage","text":"done"}
        ]}]}});
        let entries = historical_items(&data).join("");
        assert!(entries.contains("You: hello"));
        assert!(entries.contains("$ cargo test"));
        assert!(entries.contains("Codex: done"));
        assert!(!entries.contains("diff"));
    }

    #[test]
    fn message_contents_are_extracted_from_text_parts() {
        assert_eq!(message_text(&json!({"content":[{"type":"text","text":"one"},
            {"type":"text","text":"two"}]})).as_deref(), Some("one\ntwo"));
    }
}
