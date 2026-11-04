# Navigation

How a user moves around ATOMICS: between plans, their tasks, agent sessions, the chief, settings and a session's
diff. The first part is the audit of how it worked before this change; the second is what changed. The UI is drawn
once in `atomics-ui`, so the desktop and the browser's wide layout behave the same; the phone has its own pane bar.

## Audit (before)

| Going to | How | Where it fell short |
| --- | --- | --- |
| A plan | Its tab in the top bar; the Tabs menu when the row is full; History for finished plans. | The CHIEF pill moved sideways as tabs came and went, so the bar never stood still. |
| A plan's board | **Plan** in the top bar, `cmd+b`, or the tab. | Picking a plan's tab switched the panel back to the diff, whatever the user had chosen. |
| A task | Its card on the board; `open` and `diff` buttons on the card. | A click on the card only opened or closed it. Nothing marked the card of the session on screen. |
| A session | The sidebar's AGENTS list, a tile, the tab strip, `cmd+1…9`, `cmd+j`, the notice line. | The sidebar was one flat list; which plan a session worked for, and which ones needed the user, was only in a status word. |
| The chief | The CHIEF pill (`cmd+o`); on the phone a Chief pane in the bottom bar. | The phone and the desktop put it in different places. |
| A diff | **Show diff**/**Hide diff**, `cmd+d`, a tile's title, `shift+↓/↑`, the explorer. | Three buttons (Show diff, Plan, Tickets) toggled one panel in three different ways. |
| Settings | The Settings menu (desktop), ⋯ (browser and phone). | — |
| Where am I | The selected tab, the active tile's header, the diff panel's title. | No single line said plan › task › session. |
| Back | — | No way back to the place before: opening a task's diff from the board lost the board. |

## Result

- **The CHIEF pill** has a fixed width for each of three sizes, picked by the window's width alone, and always sits
  in the middle of the top bar. The brand and the plan tabs share the half on its left, the actions the half on its
  right; what does not fit folds into the **Tabs** and **Actions** menus. On the phone the pill sits in the middle of
  the top bar too; a tap opens the chief's pane, a second tap goes back to the pane before it.
- **One view switcher** (Board, Diff, Tickets) replaces Show diff, Plan and the Tickets item. The view on screen is
  filled; picking it again hides the panel. `cmd+b`, `cmd+d` and `cmd+k` do the same as its segments. Picking a plan's
  tab keeps the view; each window remembers its choice (the desktop in its store, a browser in `localStorage`). The
  phone's pane bar shows Board and Diffs with the same icons and counts.
- **The location line** over the agents and the panel says where the window is: the plan › the task the active
  session works on › the session and its status (or the worktree whose diff shows). Each part is a link: the plan
  and the task to the plan's board, the session to its terminal and diff.
- **Back and forward** walk the places the window showed (tab, session or worktree, view): the ‹ › arrows at the start
  of the location line, `cmd+alt+left` / `cmd+alt+right` (`[keys] back`, `forward`), and **Escape** while the keyboard
  is in no terminal, text field, menu or dialog, so it never takes an Escape meant for an agent. Each window keeps
  its own history (client state, `atomics_ui::nav`).
- **A task card's click** shows its session with its diff, or a finished task's diff; its title still opens and
  closes the card. The card of the session on screen is outlined in the accent color.
- **The sidebar** groups the sessions under the plan they work for (the heading shows the plan's tab), then those
  outside plans. A session that needs the user has a warning bar and its state in the warning color, with the number
  of permission requests waiting; the active one keeps the accent fill.
- **Keyboard focus is visible** on the custom controls: the pill, the switcher's segments, the location's arrows and
  links, the sidebar's rows and the task cards draw a focus ring in the theme's selection color.

The phone keeps its panes: each pane's header says where it is (the board's plan, the tile's session), and the
browser's own Back stays the page's.
