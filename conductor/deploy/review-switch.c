/*
 * conductor-review-switch: runs the review session as the reviewer uid.
 *
 * The pod's only process user is uid 1000, with no sudo. This binary carries
 * the file capabilities cap_setuid and cap_setgid (set in the image, see
 * infra/Dockerfile.conductor) and can do exactly one thing: become uid/gid
 * 1001 and exec /usr/local/bin/conductor-review-session with a clean
 * environment. The target uid is compiled in, so it cannot be told to become
 * root or any other user, and only the agent uid and group may run it (the
 * file is root:agent 0750, and it checks its real uid too).
 *
 * After the switch every id is 1001, the capabilities are cleared explicitly
 * (a switch between two non-root uids does not clear them) and no_new_privs is
 * set so nothing the session runs can gain privileges again.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <grp.h>
#include <linux/capability.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef AGENT_UID
#define AGENT_UID 1000
#endif
#ifndef REVIEWER_UID
#define REVIEWER_UID 1001
#endif
#ifndef REVIEWER_HOME
#define REVIEWER_HOME "/data/review/home"
#endif
#ifndef REVIEWER_IDENTITY
#define REVIEWER_IDENTITY "/data/review/identity"
#endif
#ifndef SESSION_PROGRAM
#define SESSION_PROGRAM "/usr/local/bin/conductor-review-session"
#endif

static const char *const PASS[] = {
    "TERM", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "COLORTERM",
    "CARGO_TARGET_DIR", "CONDUCTOR_HOOK_PORT", "CONDUCTOR_REVIEW_REPO",
    "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN",
    "DISABLE_AUTOUPDATER", "CLAUDE_CODE_VERSION",
};

/*
 * Changing between two non-root uids does not clear the capabilities the file
 * granted, so they are cleared here explicitly: permitted, effective and
 * inheritable to nothing, and the ambient set emptied.
 */
static void drop_all_capabilities(void) {
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct data[2];
    memset(data, 0, sizeof data);
    if (syscall(SYS_capset, &header, data) != 0) {
        fprintf(stderr, "conductor-review-switch: capset: %s\n", strerror(errno));
        exit(111);
    }
    prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0);
}

static void fail(const char *what) {
    fprintf(stderr, "conductor-review-switch: %s: %s\n", what, strerror(errno));
    exit(111);
}

int main(int argc, char **argv) {
    if (getuid() != AGENT_UID || geteuid() != AGENT_UID) {
        fprintf(stderr, "conductor-review-switch: only uid %d may run this\n", AGENT_UID);
        return 111;
    }

    char *env[32];
    size_t n = 0;
    static char home[] = "HOME=" REVIEWER_HOME;
    static char identity[] = "CONDUCTOR_REVIEW_IDENTITY_DIR=" REVIEWER_IDENTITY;
    static char path[] = "PATH=/usr/local/bin:/usr/local/cargo/bin:/usr/bin:/bin";
    static char user[] = "USER=reviewer";
    static char logname[] = "LOGNAME=reviewer";
    static char shell[] = "SHELL=/bin/bash";
    env[n++] = home;
    env[n++] = identity;
    env[n++] = path;
    env[n++] = user;
    env[n++] = logname;
    env[n++] = shell;
    for (size_t i = 0; i < sizeof PASS / sizeof PASS[0] && n < 31; i++) {
        const char *value = getenv(PASS[i]);
        if (value == NULL) continue;
        size_t len = strlen(PASS[i]) + strlen(value) + 2;
        char *entry = malloc(len);
        if (entry == NULL) fail("malloc");
        snprintf(entry, len, "%s=%s", PASS[i], value);
        env[n++] = entry;
    }
    env[n] = NULL;

    if (setgroups(0, NULL) != 0) fail("setgroups");
    if (setresgid(REVIEWER_UID, REVIEWER_UID, REVIEWER_UID) != 0) fail("setresgid");
    if (setresuid(REVIEWER_UID, REVIEWER_UID, REVIEWER_UID) != 0) fail("setresuid");
    drop_all_capabilities();
    if (getuid() != REVIEWER_UID || geteuid() != REVIEWER_UID || getgid() != REVIEWER_UID) {
        fprintf(stderr, "conductor-review-switch: the switch did not take effect\n");
        return 111;
    }
    if (setuid(0) == 0) {
        fprintf(stderr, "conductor-review-switch: regained root after the switch\n");
        return 111;
    }
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) fail("no_new_privs");
    umask(0);

    char *args[argc + 1];
    args[0] = (char *)SESSION_PROGRAM;
    for (int i = 1; i < argc; i++) args[i] = argv[i];
    args[argc] = NULL;
    execve(SESSION_PROGRAM, args, env);
    fail("exec " SESSION_PROGRAM);
    return 111;
}
