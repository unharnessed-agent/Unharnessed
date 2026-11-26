# uh-tui

The terminal UI: `unharnessed` in a terminal opens a session on the screen,
over an agent process of its own, as the VS Code extension does. It also
replays a recorded conversation on the same screen.

## Scope

- **The screen** (`view.rs`): full screen, on the extension's page
  (`vscode/webview/styles.css`, the dark theme: the same greys, ink and
  cyan). The conversation on the left: the user's message, the model's
  reasoning (a window of its newest lines while it thinks, folded into
  "Thought for 6s" as soon as it is over), the answer as Markdown (headings, emphasis,
  lists, quotes, code, tables with their columns aligned), each Python cell as one line
  (its mark, what it runs, the exception it raised), cells in a row folded
  into "Ran 3 Python cells", a done turn's work folded into
  "Worked for 23s · 4 cells", where the conversation was compacted a line
  across it ("Compacted · 12k tokens summarized"), which opens on the
  summary. The conversation runs the column's whole width. The kernel's
  panel on the right, on screens of 100 columns and more: the servers the
  agent left running, a row each with its ports, its time and what stops
  it, and the variables of its REPL, a row each with its name and its
  type. The `»` at its top right closes it to a rail at the screen's edge
  (a dot on it while a server runs), and the conversation takes its room;
  a click anywhere on the rail opens it again.
  Under the conversation, the message box: what it asks for under the
  caret, then the message as it is written, and its bar with the model,
  the reasoning effort as a track lit up to its level, how much of the
  context is used and, while the agent works, the mark of what stops it.
  On top of it, while the conversation has a goal, the goal.
  On the wordmark's row, in a live session, what leads to a new
  conversation, to the saved ones and to the settings.
  Surfaces are whole cells, so their corners are square in every terminal
  and font; a terminal has no pictures, so a Python cell's mark is `py` in
  Python's blue and a command's is `>_`; reasoning has `✦`, a turn's
  folded work `≡`.
- **The message** (`draft.rs`): what is typed or pasted goes into the
  message box, which grows with it up to a part of the screen. The caret
  moves by grapheme cluster, by word (`Alt+←` `Alt+→`), to its line's ends
  (`Home` `End`, `Ctrl+A` `Ctrl+E`) and through the rows; `Ctrl+W` removes
  a word, `Ctrl+U` the line up to the caret; `Alt+Enter` or `Ctrl+J`
  breaks the line, `Shift+Enter` too where the terminal tells it from
  Enter (the kitty keyboard protocol, asked of every terminal). A click in
  the message puts the caret there. A paste comes whole (bracketed paste),
  its line ends kept, cleaned like any text from outside. In a live session
  Enter sends it; a replay sends nothing.
- **The mentions** (`mention.rs`, `files.rs`), as the extension has them
  (`vscode/src/mentions.ts`, `files.ts`, `vscode/webview/mentions.tsx`): in
  a live session an `@` that starts a word opens the session's folder's
  files over the message box (the ones git does not ignore; outside a
  repository, the ones that are not hidden; listed again each time the menu
  opens), those that match what follows it first by name, then anywhere in
  the path, then by its letters in order. The arrows choose one, Enter, Tab
  or a click picks it, Esc dismisses the menu until the message changes. A
  mention of a file or a folder the folder has lies on the accent, in the
  message box and in the messages sent; a backspace right after one removes
  it whole. A message goes to the agent with a link (`resource_link`) for
  each mention that names something, and a conversation that comes back
  shows its messages without the links the agent kept under them.
- **The commands** (`slash.rs`), as the extension's `/` menu
  (`vscode/webview/Composer.tsx`): in a live session a `/` first in the
  message opens, over the message box, what the agent lists
  (`available_commands_update`): `/compact` and `/goal`, the prompt
  templates, the skills under the name the agent has for them
  (`/skill:<name>`; its built-in ones are its own business), then the
  screen's own (`/new`, `/history`, `/model`, `/effort`, `/settings`,
  `/login`, `/quit`). A row each: the name, what it takes, what it does. What is
  typed keeps the ones whose name has it, those that start with it first;
  a skill is found by its short name too, `/history` and `/quit` by
  `/resume` and `/exit`. The arrows choose one; Tab writes its name and a
  space; Enter or a click does too, unless the command takes nothing more
  (one of the screen's own, or a name already typed whole), which goes at
  once. While what a command takes is written, its row stays alone, with
  what it does said whole, and an `@` in it opens the files; Enter sends.
  Esc dismisses the menu until the message changes. A command goes to
  the agent under the name it knows, which is exact: a skill typed short
  (`/tidy`) as `/skill:tidy` unless a command or a prompt has that name,
  and a name typed in another case as the agent has it. A path
  (`/tmp/notes.md`) opens no menu, and a message brought back with `↑`
  none either.
- **The images** (`message.rs`, `clipboard.rs`), as the extension's
  attachments (`vscode/webview/attachments.ts`): in a live session a paste
  that is nothing but the paths of image files (what a terminal writes for
  files dropped on it: whole paths, `~/`, `file://`, a space quoted or
  after a backslash, the narrow space macOS puts before `PM` kept)
  attaches them to the message instead of writing them; so do the paths
  VS Code's terminal types, not pastes, for a dropped file (read as one
  when they come at once), and a screenshot's thumbnail dropped while
  macOS still holds it (read at once: it is gone moments later, and said
  to be). `Ctrl+V` attaches the image on the clipboard (asked of the
  system's own tool, `osascript`, `wl-paste` or `xclip`: a terminal pastes
  text only), and so does `Cmd+V` when the terminal pastes nothing for it
  (VS Code's) or reports the key; a file copied in Finder comes as the
  file, by `Ctrl+V` or as the name VS Code pastes of it. An image is told
  by its first bytes (PNG, JPEG, GIF, WebP), not by its name, and goes by
  its file's name, or as `Image 1`, `Image 2` (the clipboard's). The
  message box shows each by its name with the `×` that
  takes it off (Backspace on an empty message takes the last one off), and
  under them what was refused and why: a file that is no image, an
  eleventh image, one past what the agent takes while it expects large
  images shrunk (`images.autoResize`: nothing here shrinks one), any image
  while the settings block them; or that the model in use reads no image
  and no image model stands in. A message goes with its images whole
  (`image` blocks), alone or with words, and shows them by their names
  under its words; a conversation that comes back shows each as `image`.
  A cell that loaded images into the context says how many.
- **A message changed** (`chat.rs`, `live.rs`), as the extension's edit of
  a sent message (`_unharnessed/session/rewind`): while the agent is idle,
  a click on a message of the conversation, or `Shift+↑` (back through
  them, `Shift+↓` forth), brings it back to the message box with its
  images; the box says so, what was being written is kept for after, and
  the conversation dims from that message's answer on: what sending takes
  away. Sent, the agent goes back to before the earlier message, the turns
  from it on leave the conversation, and the new message goes; refused, it
  shows as a turn that failed, with why. `Esc`, `Ctrl+C` or the row's `×`
  leaves the message as it was. The messages of a conversation that came
  back can be changed too.
- **The messages sent before** (`past.rs`): from the message's first row
  `↑` brings back the one sent before, and so on back; `↓` returns, down to
  what was being written. They are the conversation's own: the ones of a
  conversation that came back too, none from another.
- **The folds** (`fold.rs`): a click on a line that folds (a turn's work,
  cells in a row, a cell, reasoning that is over) opens or closes it; the
  line clicked stays where it is and the line under the pointer comes up.
  On the panel, a click on a server opens its last lines, and a click on a
  variable the start of its value, as the kernel formats it
  (`_unharnessed/kernel/preview`). A fold moves on the screen's own clock,
  so it opens over a paused replay too.
- **The panel's servers** (`kernel.rs`), as the extension's
  (`vscode/webview/Kernel.tsx`): in a live session a click on the `×` at a
  running server's row stops it (`_unharnessed/kernel/kill`): the row
  shows stopped at once, then leaves when the agent says it ended (and
  runs on if the agent refused). A click on a port opens
  `http://localhost:<port>/` in the browser (the command `BROWSER` names,
  else the platform's). With more rows than the screen, the panel keeps
  its last one to say how many are under it, and the wheel over it
  scrolls it; elsewhere the wheel scrolls the conversation.
- **The goal** (`goal.rs`), as the extension's (`vscode/webview/Goal.tsx`,
  `vscode/src/chat.ts`), from what the agent's updates say of it
  (`_meta.unharnessed.goal`): the message box's first row has its mark,
  where it stands (Goal, Paused, Blocked, Budget reached, Done, Failed),
  what it is after (blocked: why) and the `×` that ends it; the bar has
  its time, which counts while it is active. While it is active the bar's
  square, and `Esc`, stop the turn and make the goal wait (`/goal pause`);
  a goal that waits has `▶` there, which lets it go on (`/goal resume`);
  the `×` stops the turn and clears it (`/goal clear`), and one reached or
  failed just leaves the box, as it does with the next message. These
  commands go as prompts the user did not write (marked so in the frame,
  for the recording): they show no message, and go before the ones that
  wait. Each change of the goal is a line of the conversation. A goal
  comes back with its conversation.
- **What moves** (`motion.rs`): the streaming wave that brings the words in
  (the extension's `wave.ts`); the reasoning's words come in as bright as
  the answer's, then cool to their grey (from the page to grey alone, the
  front hardly shows in a terminal); the light crossing a live word, the light
  running through a variable that changed, the folds; a line that lands
  (the kernel restored, a compaction over, the effort's level chosen), its
  letters coming in one after another in the accent, then turning to
  their own ink; the compaction's rules growing out of it; the effort's
  fill running to its level. Every animation is a
  function of the conversation and a clock: a frame at a given moment is
  always the same frame.
- **The live session** (`live.rs`, `agent.rs`): it starts the agent
  (`unharnessed --mode acp`, with the launch's own options, in a process
  group of its own) and is its ACP client: `initialize`, then
  `session/new`, or `session/load` when the launch was on a saved
  conversation (`--continue`, `--resume <session>`, `--fork`), which comes
  back with its kernel's variables. Enter sends the message
  (`session/prompt`); written while the agent works, a message waits its
  turn, shown held back. `Ctrl+Enter` (where the terminal tells it from
  Enter) sends it into the turn that runs instead
  (`_unharnessed/session/steer`), and Enter on an empty message box sends
  the first message that waits there: the agent reads it before its next
  step, what it did so far folds, and its answer comes under the message.
  A message the turn refuses, or ends without taking, goes first of those
  that wait. `Esc` (or a click on the bar's mark) stops the
  agent (`session/cancel`) and brings the messages that waited back to the
  message box, the ones the turn had not taken first. `Ctrl+C` empties the message box, else stops the agent,
  else leaves; `Ctrl+D` on an empty box, `Ctrl+Q` and `/quit` leave;
  `Ctrl+O` opens or closes the conversation's folds, `Ctrl+K` closes or
  opens the panel; `Ctrl+P` opens the models, `Ctrl+T` the reasoning effort,
  `Ctrl+R` the saved conversations, `Ctrl+N` a new one, `Ctrl+S` the
  settings; `Ctrl+V` (or `Cmd+V`) attaches the clipboard's image. An open variable's value and an open server's last lines are
  asked of the kernel, again when they change. A message given at the
  launch is sent first, the launch's other messages after it. Leaving
  closes the agent's input: it saves its kernel and exits (the wait is
  told when it lasts), and the terminal says how to come back
  (`unharnessed --resume <id>`). An agent that dies is told in the
  conversation, with what it last wrote on its error output; the launch's
  own, ending before any session opened, leaves its reason on the
  terminal, exit code 1. The saved conversations, or a new one, then start
  another agent.
  With `UNHARNESSED_TUI_RECORD=<file>` the frames exchanged are recorded,
  in the format the replay plays.
- **The providers** (`providers.rs`, `connect.rs`): their screen takes the
  conversation's place, as in the extension (`vscode/webview/Providers.tsx`,
  `SignIn.tsx`), over the agent's `_unharnessed/providers/*` methods. A new
  conversation that nothing can answer (no provider connected, and the
  session's model is one of theirs) opens on it; `/login` opens it, and so
  does a click on "Connect a provider", which stands in the bar for the
  model meanwhile. The providers come in groups: connected, the servers of
  this machine, subscriptions, API keys; what is typed filters them. A row
  opens on its key (a field that shows dots; saved with `set_api_key`), on
  the question before a sign-in replaces a credential, or straight on its
  browser sign-in; "Disconnect" asks, then removes the credential
  (`logout`, `local_remove`). A sign-in (`login`) has a screen of its own,
  each step as the agent tells it (`login_update`): the page to open
  (`login_open`: the agent opens it, nothing does unasked) with its address
  in full and what copies it through the terminal (OSC 52), a device code
  to enter, what the provider asks to be written (`login_answer`), its
  other way behind a link (the address the browser ended on, a Prime key),
  the teams to choose from; it stops with `login_cancel`, and a failure
  stays with its reason until left or tried again. Once a credential
  changed, the agent moves the session to a model it can use and the bar
  follows. The arrows choose a row, Enter opens it (the first the filter
  leaves, when none is chosen) or presses the form's button, Tab goes
  round the buttons, Esc steps back (the form, the filter, then the
  conversation), a click on the screen's title leaves it. A key and what a
  sign-in was answered are left out of a recording.
- **The settings** (`settings.rs`, `prefs/`), as the extension's
  (`vscode/webview/Settings*.tsx`, `vscode/src/settings.ts`,
  `instructions.ts`), over the agent's `_unharnessed/settings/*`, `mcp/*`
  and `skills/*` methods: "Settings" on the wordmark's row, `Ctrl+S` or
  `/settings` opens their screen in the conversation's place, a tab each.
  *General*: compaction, retries, switching providers, usage limits,
  images; a switch flips at Enter, Space or a click, a number is typed in
  its unit in its row (seconds and hours are stored as the file's
  milliseconds; emptied, the default is back and the key leaves the file),
  a model is picked from a list under its row (the image model among those
  that read images); a group's other settings dim while its switch is off.
  *MCP*: the user's servers (the `mcpServers` setting, each with how it
  stands, where it is and its switch; a form to add or change one, reached
  by a URL with its sign-in or by a command with the variables it gets,
  what the form does not show of an entry kept; removed once asked again),
  the catalog's services signed in to, and the Marketplace, searched,
  whose services are added by a token (written as dots, left out of a
  recording) or by a sign-in, which runs on the providers' sign-in screen
  and comes back. *Skills*: the skills the agent finds, the user's with a
  switch, a form that makes one (its file is told), the folders added to
  the ones it reads. *Instructions*: the groups of the
  `modelInstructions` setting, each a list of models to check and a text;
  the models of providers that are not connected stay in the file unseen.
  A change is written at once and shows before the agent answers; refused,
  why shows under its row. What the agent reads when it starts is told
  from what its first answer said: "Restart agent" then starts another
  agent on the conversation, the settings staying on the screen. The
  arrows choose a row, Enter acts on it, Tab goes through the tabs (in a
  form: through its fields and buttons), Esc steps back.
- **The model and the effort** (`pick.rs`, `models.rs`), as the
  extension's menus (`vscode/src/session.ts`, `vscode/webview/models.tsx`,
  `Effort.tsx`), over the session's `model` and `thought_level` options
  (`session/set_config_option`). A click on the bar's model, `Ctrl+P` or
  `/model` opens the models over the message box: each with its
  provider's name, its context window and what a million tokens cost
  (none where a subscription pays), the provider in use first, the model
  in use with the check. What is typed searches them (name, id,
  provider), the arrows choose one, Enter or a click picks it. A click on
  the bar's effort, `Ctrl+T` or `/effort` opens the model's levels on a
  track filled in the accent up to the one in use: `←` `→` set it a level
  over at once, a click on a level sets
  that one; a model with one level has nothing to open. Esc, or a click
  beside it, closes a menu. The screen shows the choice at once, and what
  the agent answers then: the effort follows the model, and a change it
  refuses is put back and told in the conversation.
- **The saved conversations** (`history.rs`), as the extension's history
  menu (`vscode/src/history.ts`, `vscode/webview/History.tsx`): "History"
  on the wordmark's row, `Ctrl+R`, `/resume` or `/history` opens their
  screen in the conversation's place, and `--resume` alone starts on it.
  The folder's conversations (`session/list`), the newest first, each
  with the start of its first message and how long ago it was written to;
  the open one has the check; from eight on a field filters them, and
  what is typed always does. The arrows choose a row, Enter or a click
  opens it; Delete, or a click on the row's "Delete", asks once more on
  the row, then deletes it with its kernel's saved state
  (`_unharnessed/sessions/delete`). "New chat", `Ctrl+N` or `/new` starts
  a new conversation (not from one nothing was asked in). There is one
  conversation at a time: leaving one lets go of its agent, which stops
  its turn, saves its kernel and exits on its own while another starts
  (with the launch's options, without what chose the launch's
  conversation); what waited to be sent comes back to the message box.
  One whose agent has not exited yet waits for it, and says so.
- **The conversation** (`chat.rs`, `kernel.rs`): built from the ACP frames
  the extension reads (`session/update`, `_unharnessed/kernel/changed` and
  `restored`, the answers to `session/new`, `session/load`,
  `session/prompt`, `_unharnessed/session/rewind`,
  `_unharnessed/kernel/tail`, `kill` and `preview`), a
  part of what `vscode/src/chat.ts` handles, its compactions with it (the
  usage's `compacting`, then `_meta.unharnessed.compaction`: one that did
  not run leaves). A conversation that comes back shows settled: nothing of it
  moves in but the kernel's line, and its turns carry no time (how long
  they took was not kept).
  A new agent is a new conversation (`initialize` starts it from nothing),
  so a recording of several replays to the last.
- **Text** (`text.rs`, `markdown.rs`, `cell.rs`): one glyph per grapheme
  cluster with the cells it fills; whatever comes from outside is cleaned
  before it is drawn (no escape sequence, no control character reaches the
  terminal); rows are wrapped here, never by the terminal.
- **The terminal** (`canvas.rs`, `desk.rs`): the views paint a grid of
  cells in true colours; it goes to the terminal through ratatui's buffer,
  so only the cells that changed are written, each frame whole
  (synchronized updates), in 24-bit colours or the nearest of the 256
  (`COLORTERM`). A turn that no longer moves is laid out once: its rows
  are kept while it is in view, their count while it is not, so a long
  conversation costs a frame no more than a short one. The alternate
  screen, raw mode and the mouse are given back on exit and on a panic.
- **The replay** (`unharnessed --tui-replay <recording>`): plays a
  recording at the pace it happened. Letters go to the message box, so
  the replay's keys are chords: `Ctrl+P` pauses, `Shift+←` `Shift+→` seek,
  `Ctrl+R` restarts, `Ctrl+O` opens or closes the conversation's folds,
  `Ctrl+K` closes or opens the panel (a click on it too), `Ctrl+C`,
  `Ctrl+Q` or `Esc` leaves; `PgUp`
  `PgDn` and the wheel scroll (`↑` `↓` too, from the message's first and
  last rows; `Home` `End` while it is empty), a click opens or closes a
  fold (`--speed`,
  `--from`, `--paused`, `--open`, `--no-panel`). The screen shows none of
  these keys. With `--at <seconds>[,...]` it prints the screen
  at those moments instead, as text or with its colours (`--ansi`), at
  `--size <columns>x<rows>`: no terminal needed. A recording is an ACP
  trace (`scripts/unharnessed-tui-record.py` writes one from the real
  binary, asking for the servers' last lines and the variables' values as
  an open panel does; `recordings/demo.out` is one).

## Non-goals, for now

On the providers' screen, a server of this machine (Ollama, LM Studio)
cannot be added or have its models picked again, only removed; the
credentials another agent changed meanwhile show once the screen is opened
again. No fast mode switch. On the settings' screen: a skill's file is
named, not opened (no editor is started from here); the compaction's token
counts are not shown (as in the extension); a form's fields beyond the
ones it shows (a server's headers, its tool filters) are kept, not
edited; the rows are not the wheel's to choose, and a list of models in a
form shows eight at a time. A restart stops the turn that runs, as leaving
a conversation does. An image is never shrunk here: one past the agent's
limit is refused while `images.autoResize` is on. Images show by their
names, not as pictures, and a conversation that comes back has lost the
names. `Cmd+V` on an image alone does nothing in a terminal that sends
nothing for it (Terminal.app): `Ctrl+V` does. A message that waits its turn cannot be changed, only brought back
with the others by `Esc`. A goal's commands sent from the
box's buttons come back as the user's messages with a saved conversation
(the agent keeps them as prompts), and its time is counted from what the
agent last said of it, not from the clock: a goal left active overnight
does not count the night. A conversation left
does not go on working in the background, as the extension's do: its
turn stops. The saved conversations are the session's folder's only, and
none says that another process holds it before it is opened. An agent
that stopped is not started again on its conversation: the saved
conversations bring it back. The `/` menu leaves out the agent's
`/autonomous`, as the extension's does (it runs typed), and shows a dozen
rows at a time: the arrows bring the others in, the wheel does not. A
skill's command comes back, with a saved conversation, as what the agent
made of it (its `<skill>` block, also the conversation's title among the
saved ones): the agent keeps no trace of what was typed. The messages sent
in other conversations are
not brought back. The `@` menu offers files, not folders (a folder typed
whole is a mention all the same). A cell's kind (edit, write, read,
search) and an edit's changed lines are not read from its code yet
(`vscode/src/cell.ts`). A
command the agent left running that listens on no port is not on the
panel (as in the extension), so it cannot be stopped from it; the panel's
rows are the mouse's only. One theme, the dark one. The mouse is the application's: the terminal's own selection
needs the key the terminal has for that (Fn in Terminal.app, Option in
iTerm2), and there is no selection or copy of the application's own yet. No
real logo: that takes a font that carries it, or a terminal that draws
images.

## Public API

- `live::run(Start) -> i32`: a live session in the terminal. `Start` says
  how to launch the agent (`agent::Launch`: the program, its arguments, its
  folder; and its arguments for another conversation, a new one or a saved
  one), the session's folder, whether the launch was on a saved
  conversation or left it to choose, the launch's messages and images,
  where to record, and what opens an address in the browser.
- `replay::run(args) -> i32`: the `--tui-replay` command, given what
  followed the flag.

Everything else is crate-private.

## Depends on

No other crate of the workspace: the agent is a process, started by its
path. Verifiers, both on the real binary:
`scripts/unharnessed-tui-e2e.py` (frames drawn without a terminal, and the
replay in a pseudo-terminal read by a terminal emulator, its keys and its
mouse) and `scripts/unharnessed-tui-live-e2e.py` (a live session in a
pseudo-terminal over its own agent, the faux provider answering and a real
kernel running its cells; the providers' screen and the models' menu over
the real providers, none of them reachable; a server stopped and a goal
paused, resumed and ended from the screen; the `/` menu over a prompt
template and a skill of the test's own; the settings written, a server
and a skill made, changed and removed, the Marketplace over the catalog
the repository's tests read, groups of instructions, a restart; a message
changed and the conversation gone back; images dropped, pasted from a
clipboard the test stands in for, sent and refused; `Cmd+V`, a file copied
in Finder, a path typed as VS Code does, a screenshot's thumbnail). `ignore` lists the folder's files as
git sees them, `url` writes a mention's link.
