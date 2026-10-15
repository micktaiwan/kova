/*
 * kova.h — Kova's terminal as a library (docs/embedding.md).
 *
 * Link libkova.a (cargo build --lib, in ~/.cargo/target/<profile>/) and
 * what it needs, as `cargo rustc --lib --crate-type staticlib -- --print
 * native-static-libs` lists it:
 *   -framework UserNotifications -framework AppKit -framework CoreData
 *   -framework CoreText -framework CoreImage -framework CloudKit
 *   -framework QuartzCore -framework CoreVideo -framework CoreGraphics
 *   -framework Metal -framework Foundation -framework CoreFoundation
 *   -liconv -lobjc
 *
 * Model
 *   A pane is a shell in a PTY with Kova's terminal state, named by its id
 *   (KovaPaneId), which is also the KOVA_PANE_ID its shell sees. A pane is
 *   created with or without a view. With one, the library adds its own
 *   subview to the parent NSView you give: that view takes AppKit input
 *   itself (keys, text input and accents, mouse, scroll, Edit > Copy/Paste)
 *   and draws the pane alone into its own CAMetalLayer. The typed input
 *   functions below are for a host that routes events itself; a host using
 *   the library's view does not need them.
 *
 * Clock
 *   Nothing runs on its own. The host calls, from its own timer (60 Hz is
 *   what Kova.app uses), for each pane:
 *       uint32_t ev = kova_pane_tick(pane);   // per-frame work + events
 *       if (ev & KOVA_EVENT_NEEDS_DRAW) kova_pane_draw(pane, 0);
 *   and kova_ipc_poll() once per tick when it serves a socket.
 *
 * Threading
 *   Every function except kova_version and kova_string_free must be called
 *   on the main thread. The pane table and the socket state belong to the
 *   thread that created them: a call from another thread sees no pane and
 *   fails. Panes with a view can only be created on the main thread.
 *
 * Conventions
 *   Functions returning int: 0 = done, -1 = unknown pane or not possible
 *   (for instance a call that needs a view on a pane without one); yes/no
 *   queries return 1/0, -1 for an unknown pane. Strings are NUL-terminated
 *   UTF-8; a string returned by a kova_*_copy_* function belongs to the
 *   caller and is freed with kova_string_free. A Rust panic never crosses
 *   into the host: the call fails instead.
 */
#ifndef KOVA_H
#define KOVA_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* A pane; 0 is never a pane. */
typedef uint32_t KovaPaneId;

/* ── Library ──────────────────────────────────────────────────────────── */

/* The library version ("1.14.0"), a static string. Any thread. */
const char *kova_version(void);

/* Free a string returned by a kova_*_copy_* function. NULL is ignored.
 * Any thread. */
void kova_string_free(char *s);

/* ── Panes ────────────────────────────────────────────────────────────── */

typedef struct KovaPaneOptions {
    /* NSView * the pane's view is added to, filling it (autoresized with it)
     * until kova_pane_set_frame. NULL: a pane without view (no drawing, no
     * pointer input) — reading, writing and the socket still work. */
    void *parent_view;
    /* Working directory of the shell; NULL = $HOME. */
    const char *cwd;
    /* A line typed into the shell once it is ready, then run (Enter); NULL =
     * none. When it exits, the shell remains, as in a Kova.app pane. The
     * host is responsible for its content: a newline in it runs a second
     * command. */
    const char *command;
    /* Extra environment, "NAME=value" strings, NULL-terminated; NULL = none.
     * TERM, TERM_PROGRAM, KOVA_SHELL_INTEGRATION, KOVA_SOCKET and
     * KOVA_PANE_ID are set by the library and win. */
    const char *const *env;
    /* Initial grid of a pane without view; 0 = Kova's configured default.
     * A pane with a view takes the size of its view. */
    uint16_t cols;
    uint16_t rows;
} KovaPaneOptions;

/* Create a pane and spawn its login shell ($SHELL) in a new PTY. The shell's
 * KOVA_SOCKET is the socket this thread serves (kova_ipc_start, so start it
 * first) or /tmp/kova-<pid>.sock. Appearance, font and terminal key bindings
 * come from Kova's config (~/.config/kova/config.toml), read once.
 * options may be NULL (no view, defaults). Returns 0 on failure. */
KovaPaneId kova_pane_new(const KovaPaneOptions *options);

/* Close a pane: its view leaves its parent, its shell is hung up (SIGHUP).
 * The last pane with a view frees the shared Metal context. Reaping is
 * asynchronous: on a background thread, the shell gets SIGHUP, SIGTERM,
 * then SIGKILL, and the job in its foreground SIGHUP, then SIGKILL after
 * 2 s (Claude Code ignores SIGHUP). When this returns they may still run,
 * and a process that quits without kova_shutdown can leave them behind. */
int kova_pane_free(KovaPaneId pane);

/* Quit the library: kova_pane_free on every pane of this thread, then
 * kova_ipc_stop, then wait until the shell and foreground job of every pane
 * freed so far (by this call or earlier) are gone — at most about 2 s, when
 * a job ignores SIGHUP. Call it when the host quits (for instance from
 * applicationWillTerminate:), or the panes' processes can outlive it.
 * Returns how many panes were open; the library stays usable afterwards. */
int kova_shutdown(void);

/* The pane's view (NSView *, not retained: valid until kova_pane_free), or
 * NULL for a pane without view. It accepts first responder. */
void *kova_pane_view(KovaPaneId pane);

/* Set the view's frame, in points, in its parent's coordinates (the parent's
 * own flippedness applies). The grid follows. -1 without a view. Setting the
 * view's frame directly through AppKit works the same. */
int kova_pane_set_frame(KovaPaneId pane, double x, double y, double width, double height);

/* Set the grid of a pane without view (the PTY is told). -1 with a view:
 * its grid follows its frame. */
int kova_pane_resize(KovaPaneId pane, uint16_t cols, uint16_t rows);

/* The current grid; either pointer may be NULL. */
int kova_pane_get_size(KovaPaneId pane, uint16_t *cols, uint16_t *rows);

/* ── Clock and drawing ────────────────────────────────────────────────── */

#define KOVA_EVENT_TITLE_CHANGED    (1u << 0) /* kova_pane_copy_title changed */
#define KOVA_EVENT_EXITED           (1u << 1) /* the shell exited (once) */
#define KOVA_EVENT_AWAITING_CHANGED (1u << 2) /* kova_pane_is_awaiting changed */
#define KOVA_EVENT_CLOSE_REQUESTED  (1u << 3) /* socket: close-pane / close-tab */
#define KOVA_EVENT_FOCUS_REQUESTED  (1u << 4) /* socket: focus-pane */
#define KOVA_EVENT_NEEDS_DRAW       (1u << 5) /* kova_pane_draw would draw */

/* The per-frame work of a pane — run the command once the shell is ready,
 * auto-scroll a selection dragged past the edge, probe the foreground
 * process / Claude session / waiting flag (every 0.5 s), the git branch of
 * the status bar (every 2 s), acknowledge bell and completion while the view
 * is focused — then what changed since the previous call (KOVA_EVENT_*).
 * CLOSE_REQUESTED and FOCUS_REQUESTED are requests: the library neither
 * closes nor focuses anything, the host does (kova_pane_free, showing the
 * note and making the view first responder). 0 for an unknown pane. */
uint32_t kova_pane_tick(KovaPaneId pane);

/* 1 when kova_pane_draw would present a frame: the terminal changed, the
 * cursor blinked, focus or size changed, the shell is still loading.
 * Always 0 for a pane without view. Changes nothing. */
int kova_pane_needs_draw(KovaPaneId pane);

/* Draw the pane into its layer if it needs it (always when force != 0) and
 * present the frame. 1 when a frame was presented, 0 when not (nothing
 * changed, no view, no drawable available). */
int kova_pane_draw(KovaPaneId pane, int force);

/* ── Input (for hosts that route events themselves) ───────────────────── */

#define KOVA_MOD_SHIFT   (1u << 0)
#define KOVA_MOD_CONTROL (1u << 1)
#define KOVA_MOD_OPTION  (1u << 2)
#define KOVA_MOD_COMMAND (1u << 3)

/* A key press, as NSEvent gives it. */
typedef struct KovaKeyEvent {
    uint16_t keycode;                          /* NSEvent.keyCode */
    uint32_t mods;                             /* KOVA_MOD_* */
    const char *characters;                    /* NSEvent.characters, may be NULL */
    const char *characters_ignoring_modifiers; /* NSEvent.charactersIgnoringModifiers, may be NULL */
} KovaKeyEvent;

#define KOVA_KEY_SENT    0 /* written to the PTY (kitty mode, modified key) */
#define KOVA_KEY_COMPOSE 1 /* run it through your text input (interpretKeyEvents:),
                              then kova_pane_text for committed text or
                              kova_pane_send_key for doCommandBySelector: */
#define KOVA_KEY_HOST    2 /* a Kova.app window key (Ctrl+F, Ctrl+Option+arrows):
                              nothing was sent */

/* A key press. Ctrl+L also clears the scrollback (not in alt-screen). */
int kova_pane_key(KovaPaneId pane, const KovaKeyEvent *event);

/* A key your text input did not turn into text: encode it for the PTY
 * (arrows, Ctrl+letter, Option as Meta…). Also what a host without text
 * input calls after KOVA_KEY_COMPOSE. */
int kova_pane_send_key(KovaPaneId pane, const KovaKeyEvent *event);

/* Committed text (typed characters, a composed accent, an IME result):
 * written as is, scrolls back to the live screen, answers a waiting pane. */
int kova_pane_text(KovaPaneId pane, const char *utf8);

/* A paste: wrapped in bracketed-paste markers when the app asked for them
 * (a terminator inside the text is stripped). Text only. */
int kova_pane_paste(KovaPaneId pane, const char *utf8);

/* The text being composed (marked text); NULL or "" ends the composition.
 * Kept by the pane's input component, never sent nor drawn. */
int kova_pane_preedit(KovaPaneId pane, const char *utf8);

#define KOVA_MOUSE_PRESS   0
#define KOVA_MOUSE_DRAG    1
#define KOVA_MOUSE_RELEASE 2
#define KOVA_MOUSE_MOVE    3

#define KOVA_MOUSE_LOCAL      0 /* not for the app: selection started/extended/ended */
#define KOVA_MOUSE_REPORTED   1 /* the app asked for mouse reports and got one */
#define KOVA_MOUSE_AUTOSCROLL 2 /* drag selection; past the edge, tick scrolls it */

/* A mouse event at (x, y) points from the view's top-left corner; button
 * 0 left, 1 middle, 2 right; click_count 2 selects a word, 3 a line. A
 * release that ends a selection copies it to the general pasteboard. -1
 * without a view (the cell size comes from it). mods: Shift, Control and
 * Option go into the mouse reports of an app that asked for them;
 * KOVA_MOD_COMMAND is ignored here and in kova_pane_scroll: Kova.app's
 * Cmd+hover / Cmd+click on a URL belongs to its window, not to the library. */
int kova_pane_mouse(KovaPaneId pane, int action, int button, uint32_t mods,
                    double x, double y, int click_count);

/* A vertical scroll (dy > 0 toward older output; precise != 0 for trackpad
 * deltas) at (x, y) points from the view's top-left corner. -1 without a
 * view. */
int kova_pane_scroll(KovaPaneId pane, double dy, int precise, uint32_t mods, double x, double y);

/* ── Reading ──────────────────────────────────────────────────────────── */

#define KOVA_TEXT_VISIBLE    0 /* the screen */
#define KOVA_TEXT_SCROLLBACK 1 /* the scrollback only */
#define KOVA_TEXT_ALL        2 /* scrollback then screen */

/* The pane's text, exactly what the socket's get-pane-content returns in
 * "text". Free with kova_string_free. */
char *kova_pane_copy_text(KovaPaneId pane, int mode, int trim_trailing_blank_lines);

/* The display title: Claude session name, else the terminal title, the
 * foreground program, the directory name, "shell". */
char *kova_pane_copy_title(KovaPaneId pane);

/* The id of the Claude Code conversation running in the pane (what
 * `claude --resume` takes), NULL when none. Refreshed by kova_pane_tick. */
char *kova_pane_copy_claude_session_id(KovaPaneId pane);

/* The selection as text, then cleared; NULL when nothing is selected.
 * Consecutive non-empty lines are joined with a space, blank lines kept:
 * what Kova.app's Cmd+C copies (the view's Edit > Copy does the same). */
char *kova_pane_copy_selection(KovaPaneId pane);

/* 1 while the shell runs, 0 once it exited. */
int kova_pane_is_alive(KovaPaneId pane);

/* 1 while the app in the pane says it waits for the user (Claude Code's
 * hooks set it through the socket), and nothing contradicts it. */
int kova_pane_is_awaiting(KovaPaneId pane);

/* 1 while the app in the pane is working: Claude Code's spinner leads its
 * terminal title (the idle asterisk does not count). What Kova.app counts in
 * its status bar's ✳N. Read on demand, no event. */
int kova_pane_is_working(KovaPaneId pane);

/* ── Socket ───────────────────────────────────────────────────────────── */

/* Serve Kova's socket protocol at path (NULL: /tmp/kova-<pid>.sock, where
 * Kite looks for Kova instances), over this thread's panes, one tab per
 * pane (tab id = pane id, window 0). Answered: list-panes, send-keys,
 * get-pane-content, count-pane-content, wait-for-completion, focus-pane and
 * close-pane / close-tab (as KOVA_EVENT_* requests), set-pane-status,
 * mark-completed, mark-started, bell, list-tabs. Everything else (window
 * and layout commands, subscribe, notify) gets an error. Requests are
 * answered only inside kova_ipc_poll. Panes created after this call get
 * KOVA_SOCKET=path. The socket is bound before this returns (a stale file at
 * path is replaced, the new one is owner-only). -1 when this thread already
 * serves a socket or the bind fails (missing directory, path too long…). */
int kova_ipc_start(const char *path);

/* Answer the requests received since the last call, and the
 * wait-for-completion requests that resolved. Returns how many requests
 * were received in this call: answered, or parked as a wait-for-completion
 * that a later call answers. Resolved waits are answered but not counted.
 * 0 when no socket is served. */
int kova_ipc_poll(void);

/* Stop answering and remove the socket file. */
int kova_ipc_stop(void);

#ifdef __cplusplus
}
#endif

#endif /* KOVA_H */
