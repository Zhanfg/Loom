// SPDX-License-Identifier: GPL-3.0-only
// Boundary-safe boot-scoped attestation helper for Loom first-stage activation.

#define _GNU_SOURCE
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define MAX_GENERATION 128
#define BOOT_ID_LENGTH 36
#define ACTIVE_MAX 512

#define CURRENT_BOOT "current-boot"
#define ACTIVE "active"
#define ATTEMPTED "attempted"
#define CONFIRMED "confirmed"

static int valid_generation(const char *value) {
    if (value == NULL || value[0] == '\0' || strlen(value) > MAX_GENERATION) return 0;
    for (const unsigned char *p = (const unsigned char *)value; *p != '\0'; ++p) {
        if (!(isalnum(*p) || *p == '.' || *p == '_' || *p == '-')) return 0;
    }
    return 1;
}

static int valid_boot_id(const char *value) {
    if (value == NULL || strlen(value) != BOOT_ID_LENGTH) return 0;
    for (size_t i = 0; i < BOOT_ID_LENGTH; ++i) {
        if (i == 8 || i == 13 || i == 18 || i == 23) {
            if (value[i] != '-') return 0;
        } else if (!isxdigit((unsigned char)value[i])) {
            return 0;
        }
    }
    return 1;
}

static int valid_action(const char *action) {
    return action != NULL &&
           (strcmp(action, "candidate") == 0 ||
            strcmp(action, "confirmed") == 0 ||
            strcmp(action, "last-good") == 0);
}

static int join_path(char *output, size_t output_size,
                     const char *directory, const char *name) {
    int written = snprintf(output, output_size, "%s/%s", directory, name);
    if (written < 0 || (size_t)written >= output_size) {
        errno = ENAMETOOLONG;
        return -1;
    }
    return 0;
}

static int mkdir_p(const char *path, mode_t mode) {
    char buffer[PATH_MAX];
    size_t length = strlen(path);
    if (length == 0 || length >= sizeof(buffer)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    memcpy(buffer, path, length + 1);
    for (char *p = buffer + 1; *p != '\0'; ++p) {
        if (*p != '/') continue;
        *p = '\0';
        if (mkdir(buffer, mode) != 0 && errno != EEXIST) return -1;
        *p = '/';
    }
    if (mkdir(buffer, mode) != 0 && errno != EEXIST) return -1;
    struct stat st;
    if (lstat(buffer, &st) != 0 || !S_ISDIR(st.st_mode)) {
        errno = ENOTDIR;
        return -1;
    }
    return 0;
}

static int fsync_directory(const char *path) {
    int fd = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    int result = fsync(fd);
    int saved = errno;
    close(fd);
    errno = saved;
    return result;
}

static int write_all(int fd, const char *buffer, size_t length) {
    while (length != 0) {
        ssize_t count = write(fd, buffer, length);
        if (count < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        if (count == 0) {
            errno = EIO;
            return -1;
        }
        buffer += (size_t)count;
        length -= (size_t)count;
    }
    return 0;
}

static int atomic_write(const char *state_dir, const char *name, const char *value) {
    char final_path[PATH_MAX];
    char temporary[PATH_MAX];
    if (join_path(final_path, sizeof(final_path), state_dir, name) != 0) return -1;
    int length = snprintf(temporary, sizeof(temporary), "%s/.%s.tmp-%ld",
                          state_dir, name, (long)getpid());
    if (length < 0 || (size_t)length >= sizeof(temporary)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    unlink(temporary);
    int fd = open(temporary, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0) return -1;
    int result = 0;
    if (write_all(fd, value, strlen(value)) != 0 || fsync(fd) != 0) result = -1;
    int saved = errno;
    if (close(fd) != 0 && result == 0) {
        result = -1;
        saved = errno;
    }
    if (result == 0 && rename(temporary, final_path) != 0) {
        result = -1;
        saved = errno;
    }
    if (result == 0 && fsync_directory(state_dir) != 0) {
        result = -1;
        saved = errno;
    }
    if (result != 0) unlink(temporary);
    errno = saved;
    return result;
}

static int remove_file_synced(const char *state_dir, const char *name) {
    char path[PATH_MAX];
    if (join_path(path, sizeof(path), state_dir, name) != 0) return -1;
    if (unlink(path) != 0 && errno != ENOENT) return -1;
    return fsync_directory(state_dir);
}

// Return: 1=regular single-line state, 0=missing, -1=corrupt/unreadable.
static int read_single_line(const char *state_dir, const char *name,
                            char *output, size_t output_size) {
    if (output_size < 3) {
        errno = EINVAL;
        return -1;
    }
    char path[PATH_MAX];
    if (join_path(path, sizeof(path), state_dir, name) != 0) return -1;
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) {
        if (errno == ENOENT) return 0;
        return -1;
    }
    struct stat st;
    if (fstat(fd, &st) != 0 || !S_ISREG(st.st_mode)) {
        close(fd);
        errno = EINVAL;
        return -1;
    }

    size_t used = 0;
    while (used < output_size - 1) {
        ssize_t count = read(fd, output + used, output_size - 1 - used);
        if (count < 0) {
            if (errno == EINTR) continue;
            close(fd);
            return -1;
        }
        if (count == 0) break;
        used += (size_t)count;
    }
    char extra;
    ssize_t extra_count;
    do {
        extra_count = read(fd, &extra, 1);
    } while (extra_count < 0 && errno == EINTR);
    int saved = errno;
    close(fd);
    errno = saved;
    if (extra_count != 0 || used == 0) return -1;

    output[used] = '\0';
    while (used > 0 && (output[used - 1] == '\n' || output[used - 1] == '\r')) {
        output[--used] = '\0';
    }
    if (used == 0 || strchr(output, '\n') != NULL || strchr(output, '\r') != NULL) return -1;
    return 1;
}

static int read_generation_state(const char *state_dir, const char *name,
                                 char output[MAX_GENERATION + 1]) {
    char temporary[MAX_GENERATION + 3];
    int result = read_single_line(state_dir, name, temporary, sizeof(temporary));
    if (result != 1) return result;
    if (!valid_generation(temporary)) return -1;
    strcpy(output, temporary);
    return 1;
}

static int read_current_boot(const char *state_dir, char output[BOOT_ID_LENGTH + 1]) {
    char temporary[BOOT_ID_LENGTH + 3];
    int result = read_single_line(state_dir, CURRENT_BOOT, temporary, sizeof(temporary));
    if (result != 1) return result;
    if (!valid_boot_id(temporary)) return -1;
    strcpy(output, temporary);
    return 1;
}

static int cmd_begin(const char *state_dir, const char *boot_id) {
    if (!valid_boot_id(boot_id)) {
        fprintf(stderr, "loom-early-attest: invalid boot id\n");
        return 2;
    }
    if (mkdir_p(state_dir, 0700) != 0) {
        perror("loom-early-attest: create state directory");
        return 1;
    }
    char value[BOOT_ID_LENGTH + 2];
    int length = snprintf(value, sizeof(value), "%s\n", boot_id);
    if (length <= 0 || (size_t)length >= sizeof(value) ||
        atomic_write(state_dir, CURRENT_BOOT, value) != 0) {
        perror("loom-early-attest: write current boot");
        return 1;
    }
    // Commit the new epoch before removing stale active. If cleanup fails, an
    // old active record still cannot match the new current-boot value.
    if (remove_file_synced(state_dir, ACTIVE) != 0) {
        perror("loom-early-attest: clear stale active record");
        return 1;
    }
    return 0;
}

static int required_owner_state(const char *state_dir, const char *action,
                                const char *generation) {
    const char *name = strcmp(action, "candidate") == 0 ? ATTEMPTED : CONFIRMED;
    char owner[MAX_GENERATION + 1] = {0};
    int status = read_generation_state(state_dir, name, owner);
    return status == 1 && strcmp(owner, generation) == 0;
}

static int cmd_activate(const char *state_dir, const char *boot_id,
                        const char *generation, const char *action) {
    if (!valid_boot_id(boot_id) || !valid_generation(generation) || !valid_action(action)) {
        fprintf(stderr, "loom-early-attest: invalid activation arguments\n");
        return 2;
    }
    char current[BOOT_ID_LENGTH + 1] = {0};
    if (read_current_boot(state_dir, current) != 1 || strcmp(current, boot_id) != 0) {
        fprintf(stderr, "loom-early-attest: boot id does not match current boot epoch\n");
        return 2;
    }
    if (!required_owner_state(state_dir, action, generation)) {
        fprintf(stderr, "loom-early-attest: recovery state does not authorize activation\n");
        return 2;
    }

    char active[ACTIVE_MAX];
    int length = snprintf(active, sizeof(active),
                          "boot_id=%s\ngeneration=%s\naction=%s\n",
                          boot_id, generation, action);
    if (length <= 0 || (size_t)length >= sizeof(active) ||
        atomic_write(state_dir, ACTIVE, active) != 0) {
        perror("loom-early-attest: write active record");
        return 1;
    }
    return 0;
}

static int parse_active(const char *state_dir, char boot_id[BOOT_ID_LENGTH + 1],
                        char generation[MAX_GENERATION + 1], char action[16]) {
    char path[PATH_MAX];
    if (join_path(path, sizeof(path), state_dir, ACTIVE) != 0) return -1;
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    struct stat st;
    if (fstat(fd, &st) != 0 || !S_ISREG(st.st_mode)) {
        close(fd);
        return -1;
    }
    char buffer[ACTIVE_MAX];
    size_t used = 0;
    while (used < sizeof(buffer) - 1) {
        ssize_t count = read(fd, buffer + used, sizeof(buffer) - 1 - used);
        if (count < 0) {
            if (errno == EINTR) continue;
            close(fd);
            return -1;
        }
        if (count == 0) break;
        used += (size_t)count;
    }
    char extra;
    ssize_t extra_count;
    do {
        extra_count = read(fd, &extra, 1);
    } while (extra_count < 0 && errno == EINTR);
    close(fd);
    if (extra_count != 0 || used == 0) return -1;
    buffer[used] = '\0';

    int have_boot = 0, have_generation = 0, have_action = 0;
    char *save = NULL;
    for (char *line = strtok_r(buffer, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        if (strncmp(line, "boot_id=", 8) == 0) {
            if (have_boot || strlen(line + 8) != BOOT_ID_LENGTH) return -1;
            strcpy(boot_id, line + 8);
            have_boot = 1;
        } else if (strncmp(line, "generation=", 11) == 0) {
            if (have_generation || strlen(line + 11) > MAX_GENERATION) return -1;
            strcpy(generation, line + 11);
            have_generation = 1;
        } else if (strncmp(line, "action=", 7) == 0) {
            if (have_action || strlen(line + 7) >= 16) return -1;
            strcpy(action, line + 7);
            have_action = 1;
        } else if (line[0] != '\0') {
            return -1;
        }
    }
    if (!have_boot || !have_generation || !have_action ||
        !valid_boot_id(boot_id) || !valid_generation(generation) || !valid_action(action)) {
        return -1;
    }
    return 0;
}

static int cmd_verify(const char *state_dir, const char *boot_id) {
    if (!valid_boot_id(boot_id)) {
        fprintf(stderr, "loom-early-attest: invalid boot id\n");
        return 2;
    }
    char current[BOOT_ID_LENGTH + 1] = {0};
    if (read_current_boot(state_dir, current) != 1 || strcmp(current, boot_id) != 0) {
        fprintf(stderr, "loom-early-attest: current boot epoch mismatch\n");
        return 3;
    }

    char active_boot[BOOT_ID_LENGTH + 1] = {0};
    char generation[MAX_GENERATION + 1] = {0};
    char action[16] = {0};
    if (parse_active(state_dir, active_boot, generation, action) != 0 ||
        strcmp(active_boot, boot_id) != 0) {
        fprintf(stderr, "loom-early-attest: no active record for this boot\n");
        return 3;
    }
    if (!required_owner_state(state_dir, action, generation)) {
        fprintf(stderr, "loom-early-attest: active record no longer authorized by recovery state\n");
        return 3;
    }
    printf("generation=%s action=%s boot_id=%s\n", generation, action, boot_id);
    return 0;
}

static int cmd_status(const char *state_dir) {
    char current[BOOT_ID_LENGTH + 1] = {0};
    int current_status = read_current_boot(state_dir, current);
    printf("current_boot=%s\n", current_status == 1 ? current : "");
    char active_boot[BOOT_ID_LENGTH + 1] = {0};
    char generation[MAX_GENERATION + 1] = {0};
    char action[16] = {0};
    if (parse_active(state_dir, active_boot, generation, action) == 0) {
        printf("active_boot=%s\nactive_generation=%s\nactive_action=%s\n",
               active_boot, generation, action);
    } else {
        printf("active_boot=\nactive_generation=\nactive_action=\n");
    }
    return 0;
}

static void usage(const char *program) {
    fprintf(stderr,
            "usage:\n"
            "  %s begin <state-dir> <boot-id>\n"
            "  %s activate <state-dir> <boot-id> <generation> <candidate|confirmed|last-good>\n"
            "  %s verify <state-dir> <boot-id>\n"
            "  %s status <state-dir>\n",
            program, program, program, program);
}

int main(int argc, char **argv) {
    if (argc == 4 && strcmp(argv[1], "begin") == 0) {
        return cmd_begin(argv[2], argv[3]);
    }
    if (argc == 6 && strcmp(argv[1], "activate") == 0) {
        return cmd_activate(argv[2], argv[3], argv[4], argv[5]);
    }
    if (argc == 4 && strcmp(argv[1], "verify") == 0) {
        return cmd_verify(argv[2], argv[3]);
    }
    if (argc == 3 && strcmp(argv[1], "status") == 0) {
        return cmd_status(argv[2]);
    }
    usage(argv[0]);
    return 2;
}
