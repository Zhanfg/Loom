// SPDX-License-Identifier: GPL-3.0-only
// Strict materializer for Loom first-stage dm-linear plans.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define ORIGIN_TOKEN "__LOOM_ORIGIN__"
#define METADATA_TOKEN "__LOOM_METADATA_DEVICE__"
#define MAX_LINE 4096

static int valid_device(const char *value) {
    if (value == NULL || value[0] != '/') return 0;
    for (const unsigned char *p = (const unsigned char *)value; *p != '\0'; ++p) {
        if (*p <= 0x20U || *p == 0x7fU) return 0;
    }
    return 1;
}

static int parse_u64(const char *text, uint64_t *value) {
    if (text == NULL || text[0] == '\0') return -1;
    errno = 0;
    char *end = NULL;
    unsigned long long parsed = strtoull(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0') return -1;
    *value = (uint64_t)parsed;
    return 0;
}

static int sync_parent(const char *path) {
    char parent[PATH_MAX];
    size_t length = strlen(path);
    if (length == 0 || length >= sizeof(parent)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    memcpy(parent, path, length + 1);
    char *slash = strrchr(parent, '/');
    if (slash == NULL) {
        strcpy(parent, ".");
    } else if (slash == parent) {
        slash[1] = '\0';
    } else {
        *slash = '\0';
    }
    int fd = open(parent, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    int result = fsync(fd);
    int saved = errno;
    close(fd);
    errno = saved;
    return result;
}

static int open_regular_nofollow(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    struct stat st;
    if (fstat(fd, &st) != 0 || !S_ISREG(st.st_mode)) {
        int saved = errno == 0 ? EINVAL : errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

static int write_all(int fd, const char *buffer, size_t length) {
    while (length != 0) {
        ssize_t written = write(fd, buffer, length);
        if (written < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        if (written == 0) {
            errno = EIO;
            return -1;
        }
        buffer += (size_t)written;
        length -= (size_t)written;
    }
    return 0;
}

static int emit_row(int fd, uint64_t start, uint64_t count,
                    const char *device, uint64_t source_start) {
    char row[MAX_LINE];
    int length = snprintf(row, sizeof(row),
                          "%" PRIu64 " %" PRIu64 " linear %s %" PRIu64 "\n",
                          start, count, device, source_start);
    if (length < 0 || (size_t)length >= sizeof(row)) {
        errno = EOVERFLOW;
        return -1;
    }
    return write_all(fd, row, (size_t)length);
}

static int materialize(const char *input_path, const char *origin,
                       const char *metadata, const char *output_path) {
    if (!valid_device(origin) || !valid_device(metadata) || strcmp(origin, metadata) == 0) {
        fprintf(stderr, "loom-early-table: invalid or identical backing devices\n");
        return 2;
    }

    int input_fd = open_regular_nofollow(input_path);
    if (input_fd < 0) {
        perror("loom-early-table: open input");
        return 1;
    }
    FILE *input = fdopen(input_fd, "r");
    if (input == NULL) {
        close(input_fd);
        perror("loom-early-table: fdopen input");
        return 1;
    }

    int output_fd = open(output_path,
                         O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW,
                         0600);
    if (output_fd < 0) {
        fclose(input);
        perror("loom-early-table: create output");
        return 1;
    }

    uint64_t expected_start = 0;
    size_t rows = 0;
    int result = 0;
    char line[MAX_LINE];

    while (fgets(line, sizeof(line), input) != NULL) {
        size_t length = strlen(line);
        if (length == 0) continue;
        if (line[length - 1] != '\n' && !feof(input)) {
            fprintf(stderr, "loom-early-table: table line exceeds %d bytes\n", MAX_LINE - 1);
            result = 2;
            break;
        }

        char start_text[32], count_text[32], kind[16], device[PATH_MAX], source_text[32], extra[2];
        int fields = sscanf(line, "%31s %31s %15s %4095s %31s %1s",
                            start_text, count_text, kind, device, source_text, extra);
        if (fields != 5 || strcmp(kind, "linear") != 0) {
            fprintf(stderr, "loom-early-table: malformed dm-linear row %zu\n", rows + 1);
            result = 2;
            break;
        }

        uint64_t start = 0, count = 0, source_start = 0;
        if (parse_u64(start_text, &start) != 0 ||
            parse_u64(count_text, &count) != 0 ||
            parse_u64(source_text, &source_start) != 0 || count == 0) {
            fprintf(stderr, "loom-early-table: invalid numeric field at row %zu\n", rows + 1);
            result = 2;
            break;
        }
        if (start != expected_start || UINT64_MAX - start < count) {
            fprintf(stderr, "loom-early-table: non-contiguous or overflowing row %zu\n", rows + 1);
            result = 2;
            break;
        }
        if (UINT64_MAX - source_start < count) {
            fprintf(stderr, "loom-early-table: source range overflow at row %zu\n", rows + 1);
            result = 2;
            break;
        }

        const char *resolved = NULL;
        if (strcmp(device, ORIGIN_TOKEN) == 0) {
            resolved = origin;
        } else if (strcmp(device, METADATA_TOKEN) == 0) {
            resolved = metadata;
        } else {
            fprintf(stderr, "loom-early-table: forbidden backing token at row %zu\n", rows + 1);
            result = 2;
            break;
        }

        if (emit_row(output_fd, start, count, resolved, source_start) != 0) {
            perror("loom-early-table: write output");
            result = 1;
            break;
        }
        expected_start = start + count;
        ++rows;
    }

    if (result == 0 && ferror(input)) {
        perror("loom-early-table: read input");
        result = 1;
    }
    if (result == 0 && rows == 0) {
        fprintf(stderr, "loom-early-table: empty table\n");
        result = 2;
    }
    if (result == 0 && fsync(output_fd) != 0) {
        perror("loom-early-table: fsync output");
        result = 1;
    }

    int saved = errno;
    if (fclose(input) != 0 && result == 0) {
        result = 1;
        saved = errno;
    }
    if (close(output_fd) != 0 && result == 0) {
        result = 1;
        saved = errno;
    }
    errno = saved;

    if (result == 0 && sync_parent(output_path) != 0) {
        perror("loom-early-table: fsync output parent");
        result = 1;
    }
    if (result != 0) unlink(output_path);
    return result;
}

static void usage(const char *program) {
    fprintf(stderr,
            "usage: %s <early.table> <origin-bdev> <metadata-bdev> <output-table>\n",
            program);
}

int main(int argc, char **argv) {
    if (argc != 5) {
        usage(argv[0]);
        return 2;
    }
    return materialize(argv[1], argv[2], argv[3], argv[4]);
}
