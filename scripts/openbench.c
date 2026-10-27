// N threads: openat(dir/i/count) + pread + close, read-only, over 1000 dirs.
// Measures how path lookup scales across cores, the cost under a file-reading
// GET. Build and run:
//     cc -O2 -pthread scripts/openbench.c -o openbench
//     for t in 1 2 4 8 16 $(nproc); do ./openbench /tmp/ob $t; done
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
static const char *root; static int per;
static int dirs[1000];
static void *work(void *arg) {
    long t = (long)arg; char buf[64];
    for (int k = 0; k < per; k++) {
        int fd = openat(dirs[(t * 7919 + k) % 1000], "count", O_RDONLY | O_NOFOLLOW);
        if (fd < 0) { perror("openat"); exit(1); }
        pread(fd, buf, sizeof buf, 0);
        close(fd);
    }
    return 0;
}
int main(int argc, char **argv) {
    if (argc != 3) { fprintf(stderr, "usage: openbench <dir> <threads (max 64)>\n"); return 2; }
    root = argv[1]; int threads = atoi(argv[2]); per = 200000 / threads;
    if (threads < 1 || threads > 64) { fprintf(stderr, "threads must be 1..64\n"); return 2; }
    char p[512]; mkdir(root, 0755);
    for (int i = 0; i < 1000; i++) {
        snprintf(p, sizeof p, "%s/%d", root, i); mkdir(p, 0755);
        snprintf(p, sizeof p, "%s/%d/count", root, i);
        int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644); write(fd, "42", 2); close(fd);
        snprintf(p, sizeof p, "%s/%d", root, i); dirs[i] = open(p, O_RDONLY | O_DIRECTORY);
    }
    pthread_t th[64]; struct timespec a, b; clock_gettime(CLOCK_MONOTONIC, &a);
    for (long t = 0; t < threads; t++) pthread_create(&th[t], 0, work, (void *)t);
    for (int t = 0; t < threads; t++) pthread_join(th[t], 0);
    clock_gettime(CLOCK_MONOTONIC, &b);
    double s = (b.tv_sec - a.tv_sec) + (b.tv_nsec - a.tv_nsec) / 1e9;
    printf("threads=%2d  %8.0f open+read+close/s\n", threads, per * threads / s);
}
