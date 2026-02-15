# Chat UI Opencode Iteration Notes

## Implemented Goals

- Timeline + input now render as a stacked right column.
- Thread selector is a collapsible left sidebar.
- User messages and agent/action messages now use different background cards.
- Pressing `Esc` twice within a short window enters timeline navigation mode.

## Additional Decisions

- Sidebar toggle key is `Ctrl+B`.
- Sidebar auto-collapses on narrow terminals (`< 92` columns) to keep the right column usable.
- Navigation mode:
  - Enter: `Esc` then `Esc` again quickly.
  - Exit: `i` (or `Esc` once).
  - Controls: `Up/Down` (or `j/k`) to move timeline selection, `Space` to expand/collapse, `Enter` to open thread links.
- Input remains read-only in resurrect-required threads, same as existing behavior.
- Filter shortcuts (`1-4`) continue to work in both normal and navigation modes.

## Visual Choices

- User cards: blue-tinted background.
- Assistant/system/tool/delegate cards: neutral dark background.
- Selected timeline item: brighter card background with bold emphasis.

