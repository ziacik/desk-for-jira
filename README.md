# Desk for Jira

A native GNOME desktop client with **one unified board across your Jira projects**.

## Features

- Jira Cloud (email + API token) and Jira Data Center / Server (personal access token)
- Credentials securely stored in the desktop keyring via Secret Service / oo7
- Select any combination of accessible Jira projects (for example ART and AFB)
- Load issues using JQL across those projects, with **Mine** (default), **Unassigned**, and **All** filters
- Build columns from the projects' real workflow statuses, independent of Jira's configured boards
- Reorder columns and explicitly merge different project statuses into one column; split them again when needed
- Local search by issue key, summary, and status
- Read issue descriptions, inspect details, and open an issue in Jira
- Save project selection, column ordering, and status mappings per Jira site
- Manual refresh, account switch and disconnect

The column editor uses the Jira status **ID plus project key**. Matching status names in two projects are *not* treated as equivalent until you map them together. Statuses without matching issues are still available as columns.

This is a read-only client. It does not modify the Jira projects, workflows, issues, or board configurations.

## Run

Requires GTK4, libadwaita, Rust and a desktop Secret Service keyring (for example GNOME Keyring).

On Arch Linux:

```bash
sudo pacman -S --needed rust gtk4 libadwaita gnome-keyring
cargo run --release
```

On first run, select **Jira Cloud** or **Jira Server / Data Center** and enter the base URL of your Jira instance (for example `https://yourcompany.atlassian.net`), plus credentials.

For Jira Cloud, create a token at https://id.atlassian.com/manage-profile/security/api-tokens . Enter your Atlassian email and API token. For Jira Server / Data Center, enter a personal access token instead (the email field is not used).

Jira Cloud issue search uses `/rest/api/3/search/jql`; Server / Data Center uses `/rest/api/2/search`. Project discovery and status lists come from the Jira platform project API. Up to 500 issue results are fetched per view for responsiveness. Jira permissions still govern what you can view.

Board preferences are saved in the user's XDG configuration directory, separately for each Jira site.

## Desktop integration

The repository includes a `.desktop` launcher and AppStream metadata in `packaging/`.

## Status

Read-only MVP. Editing issues, comments, offline sync, OAuth and release packaging are future improvements.

## License

MIT.
