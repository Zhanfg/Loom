// SPDX-License-Identifier: GPL-3.0-only
// Loom first-stage recovery protocol helper.
//
// This binary deliberately depends only on libc/kernel filesystem primitives.
// It is designed to be reusable by the future first-stage Loom host before
// Android's normal module userspace exists.

#define _GNU_SOURCE
#include <ctype.h>
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

#define MAX_GENERATION 128
#define MAX_DESCRIPTOR 16384
#define IO_BUFFER 16384

#define STATE_DESIRED "desired"
#define STATE_ATTEMPTED "attempted"
#define STATE_CONFIRMED "confirmed"
#define STATE_FAILED "failed"
#define STATE_FORCE_STOCK "force-stock"

struct sha256_ctx {
    uint32_t state[8];
    uint8_t block[64];
    size_t used;
    uint64_t bytes;
};

struct snapshot_descriptor {
    char generation[MAX_GENERATION + 1];
    char state[32];
    char shadow_sha[65];
    char extents_sha[65];
    char table_sha[65];
};

static const uint32_t sha256_initial[8] = {
    0x6a09e667U, 0xbb67ae85U, 0x3c6ef372U, 0xa54ff53aU,
    0x510e527fU, 0x9b05688cU, 0x1f83d9abU, 0x5be0cd19U,
};

static const uint32_t sha256_k[64] = {
    0x428a2f98U, 0x71374491U, 0xb5c0fbcfU, 0xe9b5dba5U,
    0x3956c25bU, 0x59f111f1U, 0x923f82a4U, 0xab1c5ed5U,
    0xd807aa98U, 0x12835b01U, 0x243185beU, 0x550c7dc3U,
    0x72be5d74U, 0x80deb1feU, 0x9bdc06a7U, 0xc19bf174U,
    0xe49b69c1U, 0xefbe4786U, 0x0fc19dc6U, 0x240ca1ccU,
    0x2de92c6fU, 0x4a7484aaU, 0x5cb0a9dcU, 0x76f988daU,
    0x983e5152U, 0xa831c66dU, 0xb00327c8U, 0xbf597fc7U,
    0xc6e00bf3U, 0xd5a79147U, 0x06ca6351U, 0x14292967U,
    0x27b70a85U, 0x2e1b2138U, 0x4d2c6dfcU, 0x53380d13U,
    0x650a7354U, 0x766a0abbU, 0x81c2c92eU, 0x92722c85U,
    0xa2bfe8a1U, 0xa81a664bU, 0xc24b8b70U, 0xc76c51a3U,
    0xd192e819U, 0xd6990624U, 0xf40e3585U, 0x106aa070U,
    0x19a4c116U, 0x1e376c08U, 0x2748774cU, 0x34b0bcb5U,
    0x391c0cb3U, 0x4ed8aa4aU, 0x5b9cca4fU, 0x682e6ff3U,
    0x748f82eeU, 0x78a5636fU, 0x84c87814U, 0x8cc70208U,
    0x90befffaU, 0xa4506cebU, 0xbef9a3f7U, 0xc67178f2U,
};

static uint32_t rotr32(uint32_t value, unsigned int bits) {
    return (value >> bits) | (value << (32U - bits));
}

static void sha256_compress(struct sha256_ctx *ctx, const uint8_t block[64]) {
    uint32_t w[64];
    for (size_t i = 0; i < 16; ++i) {
        const size_t p = i * 4;
        w[i] = ((uint32_t)block[p] << 24) |
               ((uint32_t)block[p + 1] << 16) |
               ((uint32_t)block[p + 2] << 8) |
               (uint32_t)block[p + 3];
    }
    for (size_t i = 16; i < 64; ++i) {
        const uint32_t s0 = rotr32(w[i - 15], 7) ^ rotr32(w[i - 15], 18) ^ (w[i - 15] >> 3);
        const uint32_t s1 = rotr32(w[i - 2], 17) ^ rotr32(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }

    uint32_t a = ctx->state[0];
    uint32_t b = ctx->state[1];
    uint32_t c = ctx->state[2];
    uint32_t d = ctx->state[3];
    uint32_t e = ctx->state[4];
    uint32_t f = ctx->state[5];
    uint32_t g = ctx->state[6];
    uint32_t h = ctx->state[7];

    for (size_t i = 0; i < 64; ++i) {
        const uint32_t s1 = rotr32(e, 6) ^ rotr32(e, 11) ^ rotr32(e, 25);
        const uint32_t ch = (e & f) ^ ((~e) & g);
        const uint32_t temp1 = h + s1 + ch + sha256_k[i] + w[i];
        const uint32_t s0 = rotr32(a, 2) ^ rotr32(a, 13) ^ rotr32(a, 22);
        const uint32_t maj = (a & b) ^ (a & c) ^ (b & c);
        const uint32_t temp2 = s0 + maj;
        h = g;
        g = f;
        f = e;
        e = d + temp1;
        d = c;
        c = b;
        b = a;
        a = temp1 + temp2;
    }

    ctx->state[0] += a;
    ctx->state[1] += b;
    ctx->state[2] += c;
    ctx->state[3] += d;
    ctx->state[4] += e;
    ctx->state[5] += f;
    ctx->state[6] += g;
    ctx->state[7] += h;
}

static void sha256_init(struct sha256_ctx *ctx) {
    memcpy(ctx->state, sha256_initial, sizeof(ctx->state));
    memset(ctx->block, 0, sizeof(ctx->block));
    ctx->used = 0;
    ctx->bytes = 0;
}

static int sha256_update(struct sha256_ctx *ctx, const uint8_t *data, size_t length) {
    if (UINT64_MAX - ctx->bytes < (uint64_t)length) return -1;
    ctx->bytes += (uint64_t)length;

    while (length != 0) {
        size_t room = sizeof(ctx->block) - ctx->used;
        size_t take = length < room ? length : room;
        memcpy(ctx->block + ctx->used, data, take);
        ctx->used += take;
        data += take;
        length -= take;
        if (ctx->used == sizeof(ctx->block)) {
            sha256_compress(ctx, ctx->block);
            ctx->used = 0;
        }
    }
    return 0;
}

static void sha256_final(struct sha256_ctx *ctx, uint8_t digest[32]) {
    const uint64_t bit_length = ctx->bytes * 8U;
    ctx->block[ctx->used++] = 0x80U;
    if (ctx->used > 56) {
        memset(ctx->block + ctx->used, 0, 64 - ctx->used);
        sha256_compress(ctx, ctx->block);
        ctx->used = 0;
    }
    memset(ctx->block + ctx->used, 0, 56 - ctx->used);
    for (size_t i = 0; i < 8; ++i) {
        ctx->block[56 + i] = (uint8_t)(bit_length >> (56U - (unsigned int)(i * 8U)));
    }
    sha256_compress(ctx, ctx->block);
    for (size_t i = 0; i < 8; ++i) {
        digest[i * 4] = (uint8_t)(ctx->state[i] >> 24);
        digest[i * 4 + 1] = (uint8_t)(ctx->state[i] >> 16);
        digest[i * 4 + 2] = (uint8_t)(ctx->state[i] >> 8);
        digest[i * 4 + 3] = (uint8_t)ctx->state[i];
    }
}

static int regular_nofollow_fd(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    struct stat st;
    if (fstat(fd, &st) != 0 || !S_ISREG(st.st_mode)) {
        close(fd);
        errno = EINVAL;
        return -1;
    }
    return fd;
}

static int sha256_file_hex(const char *path, char output[65]) {
    int fd = regular_nofollow_fd(path);
    if (fd < 0) return -1;
    struct sha256_ctx ctx;
    sha256_init(&ctx);
    uint8_t buffer[IO_BUFFER];
    for (;;) {
        ssize_t count = read(fd, buffer, sizeof(buffer));
        if (count < 0) {
            if (errno == EINTR) continue;
            close(fd);
            return -1;
        }
        if (count == 0) break;
        if (sha256_update(&ctx, buffer, (size_t)count) != 0) {
            close(fd);
            errno = EOVERFLOW;
            return -1;
        }
    }
    close(fd);
    uint8_t digest[32];
    static const char hex[] = "0123456789abcdef";
    sha256_final(&ctx, digest);
    for (size_t i = 0; i < 32; ++i) {
        output[i * 2] = hex[digest[i] >> 4];
        output[i * 2 + 1] = hex[digest[i] & 0x0fU];
    }
    output[64] = '\0';
    return 0;
}

static int valid_generation(const char *value) {
    if (value == NULL || value[0] == '\0') return 0;
    size_t length = strlen(value);
    if (length > MAX_GENERATION) return 0;
    for (size_t i = 0; i < length; ++i) {
        unsigned char c = (unsigned char)value[i];
        if (!(isalnum(c) || c == '.' || c == '_' || c == '-')) return 0;
    }
    return 1;
}

static int valid_sha256(const char *value) {
    if (value == NULL || strlen(value) != 64) return 0;
    for (size_t i = 0; i < 64; ++i) {
        if (!isxdigit((unsigned char)value[i])) return 0;
    }
    return 1;
}

static int equal_hex(const char *a, const char *b) {
    for (size_t i = 0; i < 64; ++i) {
        if (tolower((unsigned char)a[i]) != tolower((unsigned char)b[i])) return 0;
    }
    return 1;
}

static int join_path(char *out, size_t out_size, const char *a, const char *b) {
    int written = snprintf(out, out_size, "%s/%s", a, b);
    if (written < 0 || (size_t)written >= out_size) {
        errno = ENAMETOOLONG;
        return -1;
    }
    return 0;
}

static int join_snapshot_path(char *out, size_t out_size, const char *root,
                              const char *generation, const char *name) {
    int written = snprintf(out, out_size, "%s/%s/%s", root, generation, name);
    if (written < 0 || (size_t)written >= out_size) {
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

static int write_all(int fd, const void *buffer, size_t length) {
    const uint8_t *data = buffer;
    while (length != 0) {
        ssize_t written = write(fd, data, length);
        if (written < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        if (written == 0) {
            errno = EIO;
            return -1;
        }
        data += (size_t)written;
        length -= (size_t)written;
    }
    return 0;
}

static int atomic_state_write(const char *state_dir, const char *name, const char *value) {
    char final_path[PATH_MAX];
    char temporary[PATH_MAX];
    if (join_path(final_path, sizeof(final_path), state_dir, name) != 0) return -1;
    int written = snprintf(temporary, sizeof(temporary), "%s/.%s.tmp-%ld",
                           state_dir, name, (long)getpid());
    if (written < 0 || (size_t)written >= sizeof(temporary)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    unlink(temporary);
    int fd = open(temporary, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0) return -1;
    int result = 0;
    if (write_all(fd, value, strlen(value)) != 0 || write_all(fd, "\n", 1) != 0 || fsync(fd) != 0) {
        result = -1;
    }
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

static int remove_state_file(const char *state_dir, const char *name) {
    char path[PATH_MAX];
    if (join_path(path, sizeof(path), state_dir, name) != 0) return -1;
    if (unlink(path) != 0 && errno != ENOENT) return -1;
    return fsync_directory(state_dir);
}

// Return: 1=valid value, 0=missing, -1=corrupt/unreadable.
static int read_generation_state(const char *state_dir, const char *name,
                                 char output[MAX_GENERATION + 1]) {
    char path[PATH_MAX];
    if (join_path(path, sizeof(path), state_dir, name) != 0) return -1;
    int fd = regular_nofollow_fd(path);
    if (fd < 0) {
        if (errno == ENOENT) return 0;
        return -1;
    }
    char buffer[MAX_GENERATION + 3];
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    int saved = errno;
    close(fd);
    errno = saved;
    if (count <= 0 || count >= (ssize_t)(sizeof(buffer) - 1)) return -1;
    buffer[count] = '\0';
    while (count > 0 && (buffer[count - 1] == '\n' || buffer[count - 1] == '\r')) {
        buffer[--count] = '\0';
    }
    if (!valid_generation(buffer)) return -1;
    memcpy(output, buffer, (size_t)count + 1);
    return 1;
}

static int file_exists_nofollow(const char *path) {
    struct stat st;
    if (lstat(path, &st) != 0) return 0;
    return S_ISREG(st.st_mode);
}

static int descriptor_set_once(char *destination, size_t destination_size,
                               const char *value) {
    if (destination[0] != '\0') return -1;
    size_t length = strlen(value);
    if (length >= destination_size) return -1;
    memcpy(destination, value, length + 1);
    return 0;
}

static int parse_descriptor(const char *path, struct snapshot_descriptor *descriptor) {
    memset(descriptor, 0, sizeof(*descriptor));
    int fd = regular_nofollow_fd(path);
    if (fd < 0) return -1;
    char *buffer = calloc(1, MAX_DESCRIPTOR + 1);
    if (buffer == NULL) {
        close(fd);
        return -1;
    }
    ssize_t total = 0;
    while (total < MAX_DESCRIPTOR) {
        ssize_t count = read(fd, buffer + total, (size_t)(MAX_DESCRIPTOR - total));
        if (count < 0) {
            if (errno == EINTR) continue;
            free(buffer);
            close(fd);
            return -1;
        }
        if (count == 0) break;
        total += count;
    }
    char extra;
    ssize_t extra_count = read(fd, &extra, 1);
    close(fd);
    if (extra_count != 0) {
        free(buffer);
        errno = EFBIG;
        return -1;
    }
    buffer[total] = '\0';

    int result = 0;
    char *save = NULL;
    for (char *line = strtok_r(buffer, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        char *value = NULL;
        if (strncmp(line, "LOOM_GENERATION=", 16) == 0) {
            value = line + 16;
            result = descriptor_set_once(descriptor->generation,
                                         sizeof(descriptor->generation), value);
        } else if (strncmp(line, "LOOM_STATE=", 11) == 0) {
            value = line + 11;
            result = descriptor_set_once(descriptor->state,
                                         sizeof(descriptor->state), value);
        } else if (strncmp(line, "LOOM_SHADOW_SHA256=", 19) == 0) {
            value = line + 19;
            result = descriptor_set_once(descriptor->shadow_sha,
                                         sizeof(descriptor->shadow_sha), value);
        } else if (strncmp(line, "LOOM_EXTENTS_SHA256=", 20) == 0) {
            value = line + 20;
            result = descriptor_set_once(descriptor->extents_sha,
                                         sizeof(descriptor->extents_sha), value);
        } else if (strncmp(line, "LOOM_TABLE_SHA256=", 18) == 0) {
            value = line + 18;
            result = descriptor_set_once(descriptor->table_sha,
                                         sizeof(descriptor->table_sha), value);
        }
        if (result != 0) break;
    }
    free(buffer);
    return result;
}

static int snapshot_valid(const char *root, const char *generation) {
    if (!valid_generation(generation)) return 0;
    char descriptor_path[PATH_MAX];
    char shadow_path[PATH_MAX];
    char extents_path[PATH_MAX];
    char table_path[PATH_MAX];
    if (join_snapshot_path(descriptor_path, sizeof(descriptor_path), root,
                           generation, "descriptor.env") != 0 ||
        join_snapshot_path(shadow_path, sizeof(shadow_path), root,
                           generation, "shadow.pack") != 0 ||
        join_snapshot_path(extents_path, sizeof(extents_path), root,
                           generation, "shadow.extents") != 0 ||
        join_snapshot_path(table_path, sizeof(table_path), root,
                           generation, "early.table") != 0) {
        return 0;
    }

    struct snapshot_descriptor descriptor;
    if (parse_descriptor(descriptor_path, &descriptor) != 0) return 0;
    if (strcmp(descriptor.generation, generation) != 0) return 0;
    if (strcmp(descriptor.state, "PREPARED_NOT_ACTIVE") != 0 &&
        strcmp(descriptor.state, "CONFIRMED") != 0) return 0;
    if (!valid_sha256(descriptor.shadow_sha) ||
        !valid_sha256(descriptor.extents_sha) ||
        !valid_sha256(descriptor.table_sha)) return 0;
    if (!file_exists_nofollow(shadow_path) ||
        !file_exists_nofollow(extents_path) ||
        !file_exists_nofollow(table_path)) return 0;

    char actual_shadow[65];
    char actual_extents[65];
    char actual_table[65];
    if (sha256_file_hex(shadow_path, actual_shadow) != 0 ||
        sha256_file_hex(extents_path, actual_extents) != 0 ||
        sha256_file_hex(table_path, actual_table) != 0) return 0;
    return equal_hex(actual_shadow, descriptor.shadow_sha) &&
           equal_hex(actual_extents, descriptor.extents_sha) &&
           equal_hex(actual_table, descriptor.table_sha);
}

static int state_marker_exists(const char *state_dir, const char *name) {
    char path[PATH_MAX];
    if (join_path(path, sizeof(path), state_dir, name) != 0) return 0;
    struct stat st;
    return lstat(path, &st) == 0 && S_ISREG(st.st_mode);
}

static void print_stock(const char *reason) {
    printf("action=stock reason=%s\n", reason);
}

static void print_last_good(const char *generation, const char *reason) {
    printf("action=last-good generation=%s reason=%s\n", generation, reason);
}

static int fallback(const char *snapshots, int confirmed_status,
                    const char *confirmed, const char *reason) {
    if (confirmed_status == 1 && snapshot_valid(snapshots, confirmed)) {
        print_last_good(confirmed, reason);
    } else {
        print_stock(reason);
    }
    return 0;
}

static int cmd_arm(const char *state_dir, const char *generation) {
    if (!valid_generation(generation)) {
        fprintf(stderr, "loom-early-state: invalid generation id\n");
        return 2;
    }
    if (mkdir_p(state_dir, 0700) != 0 ||
        atomic_state_write(state_dir, STATE_DESIRED, generation) != 0 ||
        remove_state_file(state_dir, STATE_ATTEMPTED) != 0 ||
        remove_state_file(state_dir, STATE_FAILED) != 0) {
        perror("loom-early-state: arm");
        return 1;
    }
    return 0;
}

static int cmd_decide(const char *state_dir, const char *snapshots) {
    if (mkdir_p(state_dir, 0700) != 0) {
        print_stock("state-unavailable");
        return 0;
    }
    if (state_marker_exists(state_dir, STATE_FORCE_STOCK)) {
        print_stock("force-stock");
        return 0;
    }

    char desired[MAX_GENERATION + 1] = {0};
    char attempted[MAX_GENERATION + 1] = {0};
    char confirmed[MAX_GENERATION + 1] = {0};
    char failed[MAX_GENERATION + 1] = {0};
    int desired_status = read_generation_state(state_dir, STATE_DESIRED, desired);
    int attempted_status = read_generation_state(state_dir, STATE_ATTEMPTED, attempted);
    int confirmed_status = read_generation_state(state_dir, STATE_CONFIRMED, confirmed);
    int failed_status = read_generation_state(state_dir, STATE_FAILED, failed);

    if (desired_status == 0) {
        print_stock("no-desired-generation");
        return 0;
    }
    if (desired_status < 0 || attempted_status < 0 ||
        confirmed_status < 0 || failed_status < 0) {
        print_stock("state-invalid");
        return 0;
    }
    if (!snapshot_valid(snapshots, desired)) {
        return fallback(snapshots, confirmed_status, confirmed,
                        "desired-snapshot-invalid");
    }
    if (confirmed_status == 1 && strcmp(confirmed, desired) == 0) {
        printf("action=confirmed generation=%s reason=last-good\n", desired);
        return 0;
    }
    if (failed_status == 1 && strcmp(failed, desired) == 0) {
        return fallback(snapshots, confirmed_status, confirmed,
                        "candidate-quarantined");
    }
    if (attempted_status == 1 && strcmp(attempted, desired) == 0) {
        if (atomic_state_write(state_dir, STATE_FAILED, desired) != 0) {
            print_stock("quarantine-write-failed");
            return 0;
        }
        (void)remove_state_file(state_dir, STATE_ATTEMPTED);
        return fallback(snapshots, confirmed_status, confirmed,
                        "previous-attempt-unconfirmed");
    }
    if (atomic_state_write(state_dir, STATE_ATTEMPTED, desired) != 0) {
        print_stock("attempt-marker-failed");
        return 0;
    }
    printf("action=candidate generation=%s reason=first-attempt\n", desired);
    return 0;
}

static int cmd_confirm(const char *state_dir, const char *snapshots,
                       const char *generation) {
    if (!valid_generation(generation) || !snapshot_valid(snapshots, generation)) {
        fprintf(stderr, "loom-early-state: invalid snapshot or generation\n");
        return 2;
    }
    if (mkdir_p(state_dir, 0700) != 0) {
        perror("loom-early-state: confirm state");
        return 1;
    }
    char desired[MAX_GENERATION + 1] = {0};
    char attempted[MAX_GENERATION + 1] = {0};
    char confirmed[MAX_GENERATION + 1] = {0};
    int desired_status = read_generation_state(state_dir, STATE_DESIRED, desired);
    int attempted_status = read_generation_state(state_dir, STATE_ATTEMPTED, attempted);
    int confirmed_status = read_generation_state(state_dir, STATE_CONFIRMED, confirmed);
    if (desired_status != 1 || strcmp(desired, generation) != 0) {
        fprintf(stderr, "loom-early-state: confirmation does not match desired generation\n");
        return 2;
    }
    const int already_confirmed = confirmed_status == 1 && strcmp(confirmed, generation) == 0;
    const int was_attempted = attempted_status == 1 && strcmp(attempted, generation) == 0;
    if (!already_confirmed && !was_attempted) {
        fprintf(stderr, "loom-early-state: generation was not attempted; refusing confirmation\n");
        return 2;
    }
    if (attempted_status < 0 || confirmed_status < 0) {
        fprintf(stderr, "loom-early-state: recovery state is corrupt\n");
        return 2;
    }
    if (atomic_state_write(state_dir, STATE_CONFIRMED, generation) != 0 ||
        remove_state_file(state_dir, STATE_ATTEMPTED) != 0 ||
        remove_state_file(state_dir, STATE_FAILED) != 0) {
        perror("loom-early-state: confirm");
        return 1;
    }
    return 0;
}

static int cmd_force_stock(const char *state_dir, const char *value) {
    if (mkdir_p(state_dir, 0700) != 0) {
        perror("loom-early-state: force-stock state");
        return 1;
    }
    if (strcmp(value, "on") == 0) {
        if (atomic_state_write(state_dir, STATE_FORCE_STOCK, "1") != 0) {
            perror("loom-early-state: force-stock on");
            return 1;
        }
        return 0;
    }
    if (strcmp(value, "off") == 0) {
        if (remove_state_file(state_dir, STATE_FORCE_STOCK) != 0) {
            perror("loom-early-state: force-stock off");
            return 1;
        }
        return 0;
    }
    fprintf(stderr, "loom-early-state: force-stock expects on|off\n");
    return 2;
}

static void print_generation_status(const char *state_dir, const char *name) {
    char value[MAX_GENERATION + 1] = {0};
    int status = read_generation_state(state_dir, name, value);
    if (status == 1) printf("%s=%s\n", name, value);
    else if (status == 0) printf("%s=\n", name);
    else printf("%s=<invalid>\n", name);
}

static int cmd_status(const char *state_dir) {
    if (mkdir_p(state_dir, 0700) != 0) {
        perror("loom-early-state: status");
        return 1;
    }
    print_generation_status(state_dir, STATE_DESIRED);
    print_generation_status(state_dir, STATE_ATTEMPTED);
    print_generation_status(state_dir, STATE_CONFIRMED);
    print_generation_status(state_dir, STATE_FAILED);
    printf("force_stock=%d\n", state_marker_exists(state_dir, STATE_FORCE_STOCK) ? 1 : 0);
    return 0;
}

static void usage(const char *program) {
    fprintf(stderr,
            "usage:\n"
            "  %s arm <state-dir> <generation>\n"
            "  %s decide <state-dir> <snapshots-dir>\n"
            "  %s confirm <state-dir> <snapshots-dir> <generation>\n"
            "  %s force-stock <state-dir> on|off\n"
            "  %s status <state-dir>\n",
            program, program, program, program, program);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        usage(argv[0]);
        return 2;
    }
    if (strcmp(argv[1], "arm") == 0 && argc == 4) {
        return cmd_arm(argv[2], argv[3]);
    }
    if (strcmp(argv[1], "decide") == 0 && argc == 4) {
        return cmd_decide(argv[2], argv[3]);
    }
    if (strcmp(argv[1], "confirm") == 0 && argc == 5) {
        return cmd_confirm(argv[2], argv[3], argv[4]);
    }
    if (strcmp(argv[1], "force-stock") == 0 && argc == 4) {
        return cmd_force_stock(argv[2], argv[3]);
    }
    if (strcmp(argv[1], "status") == 0 && argc == 3) {
        return cmd_status(argv[2]);
    }
    usage(argv[0]);
    return 2;
}
