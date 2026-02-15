# Chat UI Opencode Iteration Notes

## Implemented Goals

- Timeline + input now renders as a stacked right column.
- Thread selector is a collapsible, left sidebar.
- User messages and agent/action messages now use different background cards.
- Pressing `Esc` twice within a short window enters timeline navigation mode.
- Timeline and input panes are borderless/titleless, matching the flatter OpenCode look.
- Input surface uses a gray background with internal vertical padding.
- User and assistant text cards include extra vertical padding.
- Visually mute `[SYSTEM]` timeline lines.
- Timeline cards now have exactly one separator line between cards.
- Render a single spacer line below the input pane (before status).
- Right pane content uses a 1-column inner horizontal margin.
- Input prompt now uses a bold right chevron (`❯`).
- Sidebar is borderless and uses simpler ASCII-style markers.
- Source tags switched from bracket labels to colored chips.

## Additional Decisions

- Sidebar toggle key is `Ctrl+B`.
- Sidebar auto-collapses on narrow terminals (`< 92` columns) to keep the right column usable.
- Navigation mode:
  - Enter: `Esc` then `Esc` again quickly.
  - Exit: `i` (or `Esc` once).
  - Controls: `Up/Down` (or `j/k`) to move timeline selection, `Space` to expand/collapse, `Enter` to open thread links.
- Input remains read-only in resurrect-required threads, same as existing behavior.
- Filter shortcuts (`1-4`) continue to work in both normal and navigation modes.
- Indicate input focus by a thin-colored left gutter line.
- Indicate card focus by a thin-colored left line on the selected timeline card.

## Visual Choices

- User cards: blue-tinted background.
- Assistant/system/tool/delegate cards: neutral dark background.
- Selected timeline item: brighter card background with bold emphasis.
