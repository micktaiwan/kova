// Smoke test of the C API (include/kova.h) from a host that is not Rust.
//
// Exercises, on the main thread of a plain Objective-C program (no Kova.app,
// no run loop): a pane without view (spawn with cwd/command/env, read back,
// resize), the socket (list-panes and get-pane-content from a client thread,
// answered by kova_ipc_poll), and a pane with a view inside an NSView that
// is in no window (the library's subview, its CAMetalLayer, a forced draw,
// a frame change that changes the grid), a socket path that cannot be bound,
// and kova_shutdown leaving no process behind (a shell and a job that both
// ignore SIGHUP, one pane freed before, one closed by it). The shell is
// /bin/sh so the user's zsh setup plays no part, and PTY capture files are
// turned off.
//
// Build and run (from the repository root):
//   cargo build --lib
//   clang -fobjc-arc -Wall -Iinclude tests/c_smoke/smoke.m \
//     ~/.cargo/target/debug/libkova.a \
//     -framework UserNotifications -framework AppKit -framework CoreData \
//     -framework CoreText -framework CoreImage -framework CloudKit \
//     -framework QuartzCore -framework CoreVideo -framework CoreGraphics \
//     -framework Metal -framework Foundation -framework CoreFoundation \
//     -liconv -o <dir>/kova-smoke
//   <dir>/kova-smoke <short dir>/smoke.sock
// (a socket path is limited to 103 bytes: a longer one fails kova_ipc_start)

#import <AppKit/AppKit.h>
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include "kova.h"

static int failures = 0;

#define CHECK(cond, ...)                                      \
    do {                                                      \
        if (cond) {                                           \
            printf("ok    ");                                 \
        } else {                                              \
            printf("FAIL  ");                                 \
            failures++;                                       \
        }                                                     \
        printf(__VA_ARGS__);                                  \
        printf("\n");                                         \
    } while (0)

static double now(void) { return [NSDate timeIntervalSinceReferenceDate]; }

// Tick the pane and serve the socket until its visible text contains `needle`.
static char *wait_for_text(KovaPaneId pane, const char *needle, double timeout) {
    double deadline = now() + timeout;
    while (now() < deadline) {
        kova_pane_tick(pane);
        kova_ipc_poll();
        char *text = kova_pane_copy_text(pane, KOVA_TEXT_ALL, 1);
        if (text && strstr(text, needle)) return text;
        kova_string_free(text);
        usleep(10 * 1000);
    }
    return kova_pane_copy_text(pane, KOVA_TEXT_ALL, 1);
}

static int alive(pid_t pid) { return kill(pid, 0) == 0 || errno == EPERM; }

// True once pid is gone, giving an orphan up to a second to be reaped.
static int gone_soon(pid_t pid) {
    double deadline = now() + 1.0;
    while (alive(pid)) {
        if (now() > deadline) return 0;
        usleep(10 * 1000);
    }
    return 1;
}

// A pane whose shell and foreground job both ignore SIGHUP, as Claude Code
// does; *shell and *job receive their pids once the job printed them.
static KovaPaneId hup_proof_pane(const char *tag, pid_t *shell, pid_t *job) {
    char command[256];
    snprintf(command, sizeof(command),
             "trap '' HUP; sh -c 'trap \"\" HUP; echo \"%s:$PPID:$$\"; exec sleep 600'", tag);
    KovaPaneOptions o = {.command = command, .cols = 80, .rows = 24};
    KovaPaneId pane = kova_pane_new(&o);
    *shell = *job = 0;
    char marker[32];
    snprintf(marker, sizeof(marker), "\n%s:", tag);
    double deadline = now() + 10.0;
    while (pane && now() < deadline) {
        kova_pane_tick(pane);
        char *text = kova_pane_copy_text(pane, KOVA_TEXT_ALL, 1);
        char *at = text ? strstr(text, marker) : NULL;
        int s = 0, j = 0;
        if (at && sscanf(at + strlen(marker), "%d:%d", &s, &j) == 2) {
            *shell = s;
            *job = j;
            kova_string_free(text);
            break;
        }
        kova_string_free(text);
        usleep(10 * 1000);
    }
    return pane;
}

// ── A socket client on its own thread, as Kite would be ──────────────────

typedef struct {
    const char *path;
    char request[512];
    char reply[65536];
    int done;
} Client;

static void *client_run(void *arg) {
    Client *c = arg;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, c->path, sizeof(addr.sun_path) - 1);
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0) {
        write(fd, c->request, strlen(c->request));
        size_t got = 0;
        while (got < sizeof(c->reply) - 1) {
            ssize_t n = read(fd, c->reply + got, sizeof(c->reply) - 1 - got);
            if (n <= 0) break;
            got += (size_t)n;
            if (c->reply[got - 1] == '\n') break;
        }
        c->reply[got] = 0;
    } else {
        snprintf(c->reply, sizeof(c->reply), "connect failed");
    }
    close(fd);
    __atomic_store_n(&c->done, 1, __ATOMIC_SEQ_CST);
    return NULL;
}

static void ask(const char *path, const char *request, char *reply_out, size_t cap) {
    static Client c;
    memset(&c, 0, sizeof(c));
    c.path = path;
    snprintf(c.request, sizeof(c.request), "%s\n", request);
    pthread_t t;
    pthread_create(&t, NULL, client_run, &c);
    double deadline = now() + 5.0;
    while (!__atomic_load_n(&c.done, __ATOMIC_SEQ_CST) && now() < deadline) {
        kova_ipc_poll();
        usleep(2 * 1000);
    }
    pthread_join(t, NULL);
    snprintf(reply_out, cap, "%s", c.reply);
}

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc < 2) {
            fprintf(stderr, "usage: %s <socket path>\n", argv[0]);
            return 2;
        }
        const char *sock = argv[1];
        setenv("SHELL", "/bin/sh", 1);
        setenv("KOVA_PTY_CAPTURE", "0", 1);

        printf("kova_version: %s\n", kova_version());
        const char *unbindable = "/nonexistent-kova-smoke-dir/kova.sock";
        CHECK(kova_ipc_start(unbindable) == -1, "kova_ipc_start(%s) fails: the bind is reported", unbindable);
        CHECK(kova_ipc_stop() == -1, "nothing served after a failed start");
        CHECK(kova_ipc_start(sock) == 0, "kova_ipc_start(%s)", sock);
        CHECK(access(sock, F_OK) == 0, "the socket file exists when kova_ipc_start returns");

        // ── A pane without view ──────────────────────────────────────────
        const char *env[] = {"SMOKE_VAR=42", "KOVA_PANE_ID=hijacked", NULL};
        KovaPaneOptions headless = {
            .parent_view = NULL,
            .cwd = "/",
            .command = "echo \"SMOKE:$KOVA_PANE_ID:$KOVA_SOCKET:$SMOKE_VAR:$(pwd)\"",
            .env = env,
            .cols = 100,
            .rows = 20,
        };
        KovaPaneId a = kova_pane_new(&headless);
        CHECK(a != 0, "kova_pane_new (no view) -> pane %u", a);
        char expected[512];
        snprintf(expected, sizeof(expected), "SMOKE:%u:%s:42:/", a, sock);
        char *text = wait_for_text(a, expected, 10.0);
        CHECK(text && strstr(text, expected), "command ran with env and cwd: found \"%s\"", expected);
        printf("----- pane %u text -----\n%s\n-----------------------\n", a, text ? text : "(null)");
        kova_string_free(text);

        uint16_t cols = 0, rows = 0;
        kova_pane_get_size(a, &cols, &rows);
        CHECK(cols == 100 && rows == 20, "kova_pane_get_size -> %ux%u", cols, rows);
        CHECK(kova_pane_resize(a, 60, 10) == 0, "kova_pane_resize(60x10)");
        kova_pane_get_size(a, &cols, &rows);
        CHECK(cols == 60 && rows == 10, "kova_pane_get_size after resize -> %ux%u", cols, rows);
        CHECK(kova_pane_text(a, "echo typed-$((6*7))\r") == 0, "kova_pane_text");
        text = wait_for_text(a, "typed-42", 5.0);
        CHECK(text && strstr(text, "typed-42"), "typed text ran in the shell");
        kova_string_free(text);
        char *title = kova_pane_copy_title(a);
        printf("title: %s\n", title);
        kova_string_free(title);
        CHECK(kova_pane_is_alive(a) == 1, "kova_pane_is_alive");
        CHECK(kova_pane_view(a) == NULL, "no view");
        CHECK(kova_pane_draw(a, 1) == 0, "nothing drawn without a view");

        // ── The socket ───────────────────────────────────────────────────
        char reply[65536];
        ask(sock, "{\"cmd\":\"list-panes\"}", reply, sizeof(reply));
        printf("list-panes -> %s", reply);
        char id_field[64];
        snprintf(id_field, sizeof(id_field), "\"id\":%u", a);
        CHECK(strstr(reply, "\"ok\":true") && strstr(reply, id_field), "list-panes lists pane %u", a);
        char request[256];
        snprintf(request, sizeof(request), "{\"cmd\":\"get-pane-content\",\"panes\":[%u]}", a);
        ask(sock, request, reply, sizeof(reply));
        printf("get-pane-content -> %.300s...\n", reply);
        CHECK(strstr(reply, "typed-42") != NULL, "get-pane-content returns the pane's text");
        snprintf(request, sizeof(request), "{\"cmd\":\"close-tab\",\"tab_id\":%u}", a);
        ask(sock, request, reply, sizeof(reply));
        printf("close-tab -> %s", reply);
        CHECK(kova_pane_tick(a) & KOVA_EVENT_CLOSE_REQUESTED, "close-tab reaches the host as KOVA_EVENT_CLOSE_REQUESTED");

        // ── A pane with a view ───────────────────────────────────────────
        [NSApplication sharedApplication];
        NSView *parent = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, 640, 400)];
        KovaPaneOptions withView = {.parent_view = (__bridge void *)parent, .command = "echo view-pane-ready"};
        KovaPaneId b = kova_pane_new(&withView);
        CHECK(b != 0, "kova_pane_new (view) -> pane %u", b);
        NSView *view = (__bridge NSView *)kova_pane_view(b);
        CHECK(view != nil && view.superview == parent, "the library's view is a subview of the parent: %s",
              view ? [[view className] UTF8String] : "(nil)");
        CHECK(view.layer != nil && [view.layer isKindOfClass:NSClassFromString(@"CAMetalLayer")],
              "its layer is a CAMetalLayer");
        kova_pane_get_size(b, &cols, &rows);
        printf("grid for 640x400 pt at scale 2 (no window): %ux%u\n", cols, rows);
        CHECK(cols > 1 && rows > 1, "the grid follows the view");
        text = wait_for_text(b, "view-pane-ready", 10.0);
        CHECK(text && strstr(text, "view-pane-ready"), "the view pane's shell ran its command");
        kova_string_free(text);
        CHECK(kova_pane_needs_draw(b) == 1, "a fresh pane needs a frame");
        int drawn = kova_pane_draw(b, 1);
        printf("kova_pane_draw(force) -> %d\n", drawn);
        CHECK(drawn == 1, "a frame was presented into the offscreen layer");
        uint16_t big_cols = cols;
        CHECK(kova_pane_set_frame(b, 0, 0, 320, 200) == 0, "kova_pane_set_frame(320x200)");
        kova_pane_get_size(b, &cols, &rows);
        printf("grid after set_frame: %ux%u\n", cols, rows);
        CHECK(cols < big_cols, "a smaller frame gives a smaller grid");
        CHECK(kova_pane_mouse(b, KOVA_MOUSE_PRESS, 0, 0, 30, 10, 1) == KOVA_MOUSE_LOCAL, "mouse press (local)");
        CHECK(kova_pane_mouse(b, KOVA_MOUSE_RELEASE, 0, 0, 30, 10, 1) == KOVA_MOUSE_LOCAL, "mouse release (local)");
        CHECK(kova_pane_scroll(b, 3.0, 0, 0, 30, 10) == 0, "scroll");

        // ── Closing ──────────────────────────────────────────────────────
        CHECK(kova_pane_free(a) == 0, "kova_pane_free(%u)", a);
        CHECK(kova_pane_free(b) == 0, "kova_pane_free(%u)", b);
        CHECK(view.superview == nil, "the view left its parent");
        CHECK(kova_pane_is_alive(a) == -1, "a freed pane is unknown");
        CHECK(kova_ipc_stop() == 0, "kova_ipc_stop");
        CHECK(access(sock, F_OK) != 0, "socket file removed");

        // ── Quitting: kova_shutdown ──────────────────────────────────────
        pid_t freed_shell, freed_job, open_shell, open_job;
        KovaPaneId c = hup_proof_pane("FREED", &freed_shell, &freed_job);
        KovaPaneId d = hup_proof_pane("OPEN", &open_shell, &open_job);
        printf("pane %u: shell %d, job %d; pane %u: shell %d, job %d\n", c, freed_shell, freed_job, d,
               open_shell, open_job);
        CHECK(freed_shell > 1 && freed_job > 1 && open_shell > 1 && open_job > 1, "both jobs printed their pids");
        CHECK(alive(freed_shell) && alive(freed_job) && alive(open_shell) && alive(open_job),
              "shells and jobs are running");
        CHECK(kova_pane_free(c) == 0, "kova_pane_free(%u)", c);
        double t0 = now();
        int closed = kova_shutdown();
        printf("kova_shutdown -> %d in %.2f s\n", closed, now() - t0);
        CHECK(closed == 1, "kova_shutdown closed the one pane still open");
        CHECK(kova_pane_is_alive(d) == -1, "pane %u is unknown after kova_shutdown", d);
        CHECK(!alive(freed_shell) && !alive(open_shell), "both shells are gone when kova_shutdown returns");
        CHECK(gone_soon(freed_job) && gone_soon(open_job), "both jobs are gone");
        CHECK(kova_shutdown() == 0, "a second kova_shutdown has nothing to do");

        printf(failures ? "SMOKE FAILED (%d)\n" : "SMOKE OK\n", failures);
        return failures ? 1 : 0;
    }
}
