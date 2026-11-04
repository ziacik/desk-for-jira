# Desk for Jira

A native GNOME desktop client to browse **your Jira issues and boards**.

## Features

- Jira Cloud (email + API token) and Jira Data Center / Server (personal access token)
- Credentials stored in the desktop keyring using Secret Service / oo7, not in plaintext files
- Your assigned tickets, with local search
- Pin a few favourite boards in the sidebar; search and select boards in **Manage boards…**
- Browse pinned boards, with a separate **Board / Backlog** switch for Kanban and Scrum boards
- Filter board and backlog tickets by assignee: **Mine** (default), **Unassigned**, or **All**; Jira filters results before pagination
- Read ranked backlog issues in a compact list; board columns only show active board tickets
- Read issue descriptions and open tickets in the browser
- Manual refresh, account switch and disconnect
- Nonblocking network requests; no Jira credentials in logs

Board selection is saved per Jira site in the desktop configuration directory. All accessible boards remain searchable via **Manage boards…**.

Read-only by design for the initial release.

## Run

Requires GTK4, libadwaita, Rust and a desktop Secret Service keyring (for example GNOME Keyring).

On Arch Linux:

```bash
sudo pacman -S --needed rust gtk4 libadwaita gnome-keyring
cargo run --release
```

On first run, select either **Jira Cloud** or **Jira Server / Data Center** and enter the base URL of your Jira instance (for example `https://yourcompany.atlassian.net`), plus credentials.

For Jira Cloud, create a token at https://id.atlassian.com/manage-profile/security/api-tokens . Enter your Atlassian email and API token. For Jira Server / Data Center, enter a personal access token instead (the email field is not used).

The app uses the current Jira Cloud `/rest/api/3/search/jql` API; Server / Data Center uses `/rest/api/2/search`. Available boards use the Jira Software Agile API. Up to 500 results are fetched per list to keep large installations responsive. Jira permissions still govern what you can view.

## Desktop integration

The repository includes a `.desktop` launcher and AppStream metadata under `packaging/`. These can be installed when packaging the application.

## Status

Initial functional MVP. Editing issues, comments, offline sync, OAuth and release packaging are future improvements.

## License

MIT.
