mod chip_wrap;
mod codex;
mod codex_session;
mod codex_ui;
mod codex_markdown;
mod command_palette;
mod dialog;
mod issue_detail;
mod issue_links;
mod jira;
mod jira_inbox;
mod git;
mod git_ui;
mod all_worktrees;
mod development;
mod pull_requests;
mod pr_review;
mod preferences;
mod secrets;
mod transition_ui;
mod ui;
mod zoom;

fn main() -> gtk::glib::ExitCode {
    ui::run()
}
