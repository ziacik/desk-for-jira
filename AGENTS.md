# Repository instructions for AI coding agents

These instructions apply to the entire repository. Favor consistency with the existing codebase over inventing new implementations.

## Before implementing anything

1. Locate at least one existing implementation of the same or closest behavior in this repository and inspect it.
2. Reuse existing widgets, helper functions, styles, patterns, and dependencies whenever practical.
3. If an established implementation cannot be reused, state the concrete technical reason in the change description.
4. Check adjacent controls and related screens for interaction, dimensions, alignment, naming, keyboard behavior, and visual consistency before coding.

## GTK4 / libadwaita UI

- Prefer native GTK4 and libadwaita widgets, properties, and theme-aware styles.
- All header/toolbar action icons must use the same GTK symbolic-icon mechanism as neighboring actions (e.g. `gtk::Button::from_icon_name`, `set_icon_name`, or `gtk::Image::from_icon_name`).
- Never manually draw a routine UI icon with Cairo, text glyphs, custom SVG-to-widget code, or a one-off `DrawingArea` when an appropriate symbolic icon can be used.
- Do not individually scale, offset, or pad icons to compensate for inconsistent icon sources. Fix the icon source or rendering path instead.
- If the icon is unavailable in the target icon theme, bundle a properly themed symbolic asset and load it through GTK's icon resource mechanism; use the standard icon size, not per-control pixel hacks.
- Prefer existing CSS classes and spacing rules. Do not introduce a unique style for one button when a shared class works.
- Avoid opening VS Code, a terminal, a browser, or creating worktrees as side effects unless the user invoked the specific action.

## Quality bar

- Keep changes as small and focused as possible. Do not add libraries, build-system machinery, or custom infrastructure for a problem with an existing idiomatic solution unless demonstrably necessary.
- Follow established file/module structure and naming conventions.
- Validate the actual modified flows, not just compilation; examine adjacent controls and both dark and light theme for visual changes where feasible.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` when the local environment supports them. Treat warnings as errors.
- If a check cannot run, disclose which check was not run. Never imply a fix was build- or visually-tested without doing so.
- Before finishing, review the diff for ad-hoc duplicate code, hardcoded pixel exceptions, and unintended side effects.

## Pull request / handoff checklist

In the PR description or final handoff, briefly answer:
- What existing pattern was reused?
- What was changed, and why?
- What validation was actually performed, and what remains unverified?

Do not mistake this document for an automated enforcement mechanism: automated tests and review are still required.
