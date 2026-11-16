mod codex;
mod codex_session;
mod codex_ui;
mod command_palette;
mod dialog;
mod issue_detail;
mod issue_links;
mod jira;
mod git;
mod git_ui;
mod all_worktrees;
mod development;
mod pull_requests;
mod preferences;
mod secrets;
mod transition_ui;
mod ui;
mod zoom;

fn main() -> gtk::glib::ExitCode {
    ui::run()
}
