// Private subprocess-only I/O injection, with no production runtime hooks.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int sync_file(int fd, const char *symbol) {
    const char *marker = getenv("NANOVM_SYNC_FAULT_FILE");
    char link[64], path[4096];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t len = readlink(link, path, sizeof(path) - 1);
    if (marker && len >= 0) {
        path[len] = 0;
        const char *init = strstr(path, "/init-sync");
        if (strstr(path, "/parent2/") || init) {
            int out = open(marker, O_WRONLY | O_APPEND | O_CLOEXEC);
            if (out >= 0) {
                // The test requires evidence that a real sync reached this hook.
                int directory = init && strcmp(init, "/init-sync") == 0;
                (void)write(out, directory ? "D" : "F", 1);
                close(out);
                if (!init || directory) {
                    errno = EIO;
                    return -1;
                }
            }
        }
    }
    int (*real_sync)(int) = dlsym(RTLD_NEXT, symbol);
    return real_sync(fd);
}

int fsync(int fd) { return sync_file(fd, "fsync"); }
int fdatasync(int fd) { return sync_file(fd, "fdatasync"); }
