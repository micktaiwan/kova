# 2.1.0

- Pause an agent from Cmd+P with `p`: its memory is freed, the shell stays, and `p` again resumes the conversation, even after a relaunch
- Auto-pause (off by default): agents idle and out of focus for `[agents] auto_pause_hours` are paused by Kova, and focusing the pane brings them back
- Reopen a closed pane with its conversation; Pi, omp and Tazu sessions resume and show up in the search palette
- Programs can show a status badge on their pane (OSC 7501), counted in the working counter; the switcher marks agents resting with background shells
- Choose a subject's color (right-click its band, or `c` in Cmd+P); the search palette scrolls with the wheel and opens a hit on click
