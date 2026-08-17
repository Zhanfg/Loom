// SPDX-License-Identifier: GPL-3.0-only
// Narrow compatibility helper for preparing Loom early-boot shadow mappings.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/fiemap.h>
#include <linux/fs.h>
#include <linux/magic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <unistd.h>

#ifndef FIEMAP_EXTENT_SHARED
#define FIEMAP_EXTENT_SHARED 0x00002000
#endif

#define SECTOR_SIZE 512ULL
#define EXTENTS_PER_BATCH 128U

static int fail_errno(const char *what) {
    fprintf(stderr, "loom-fiemap: %s: %s\n", what, strerror(errno));
    return 1;
}

static int fail_msg(const char *what) {
    fprintf(stderr, "loom-fiemap: %s\n", what);
    return 1;
}

static uint32_t rejected_extent_flags(void) {
    return FIEMAP_EXTENT_UNKNOWN |
           FIEMAP_EXTENT_DELALLOC |
           FIEMAP_EXTENT_ENCODED |
           FIEMAP_EXTENT_NOT_ALIGNED |
           FIEMAP_EXTENT_DATA_INLINE |
           FIEMAP_EXTENT_DATA_TAIL |
           FIEMAP_EXTENT_UNWRITTEN |
           FIEMAP_EXTENT_MERGED |
           FIEMAP_EXTENT_SHARED;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s <sealed-shadow-file> <extent-map-out>\n", argv[0]);
        return 2;
    }

    const char *shadow_path = argv[1];
    const char *output_path = argv[2];
    int fd = open(shadow_path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return fail_errno("open shadow file");

    struct stat st;
    if (fstat(fd, &st) != 0) {
        close(fd);
        return fail_errno("fstat shadow file");
    }
    if (!S_ISREG(st.st_mode)) {
        close(fd);
        return fail_msg("shadow path is not a regular file");
    }
    if (st.st_size <= 0 || ((uint64_t)st.st_size % SECTOR_SIZE) != 0) {
        close(fd);
        return fail_msg("shadow file size must be a positive multiple of 512 bytes");
    }

    struct statfs sfs;
    if (fstatfs(fd, &sfs) != 0) {
        close(fd);
        return fail_errno("fstatfs shadow file");
    }
    if ((unsigned long)sfs.f_type != (unsigned long)EXT4_SUPER_MAGIC) {
        close(fd);
        return fail_msg("early raw shadow backing currently requires ext4 /metadata");
    }

    FILE *out = fopen(output_path, "wx");
    if (!out) {
        close(fd);
        return fail_errno("create extent map");
    }

    const size_t bytes = sizeof(struct fiemap) +
                         EXTENTS_PER_BATCH * sizeof(struct fiemap_extent);
    struct fiemap *map = calloc(1, bytes);
    if (!map) {
        fclose(out);
        unlink(output_path);
        close(fd);
        return fail_errno("allocate FIEMAP buffer");
    }

    const uint64_t file_size = (uint64_t)st.st_size;
    const uint32_t rejected = rejected_extent_flags();
    uint64_t cursor = 0;
    int result = 0;

    while (cursor < file_size) {
        memset(map, 0, bytes);
        map->fm_start = cursor;
        map->fm_length = FIEMAP_MAX_OFFSET;
        map->fm_flags = FIEMAP_FLAG_SYNC;
        map->fm_extent_count = EXTENTS_PER_BATCH;

        if (ioctl(fd, FS_IOC_FIEMAP, map) != 0) {
            result = fail_errno("FS_IOC_FIEMAP");
            break;
        }
        if (map->fm_mapped_extents == 0) {
            result = fail_msg("FIEMAP returned a coverage gap");
            break;
        }

        int saw_last = 0;
        for (uint32_t i = 0; i < map->fm_mapped_extents && cursor < file_size; ++i) {
            const struct fiemap_extent *extent = &map->fm_extents[i];
            if (extent->fe_logical != cursor) {
                result = fail_msg("shadow file is sparse or FIEMAP is non-contiguous");
                break;
            }
            if (extent->fe_length == 0) {
                result = fail_msg("FIEMAP returned a zero-length extent");
                break;
            }
            if ((extent->fe_flags & rejected) != 0) {
                fprintf(stderr,
                        "loom-fiemap: unsupported FIEMAP extent flags 0x%08" PRIx32 "\n",
                        extent->fe_flags & rejected);
                result = 1;
                break;
            }
            if ((extent->fe_logical % SECTOR_SIZE) != 0 ||
                (extent->fe_physical % SECTOR_SIZE) != 0 ||
                (extent->fe_length % SECTOR_SIZE) != 0) {
                result = fail_msg("FIEMAP extent is not sector aligned");
                break;
            }

            uint64_t usable = extent->fe_length;
            const uint64_t remaining = file_size - cursor;
            if (usable > remaining) usable = remaining;
            if ((usable % SECTOR_SIZE) != 0) {
                result = fail_msg("final FIEMAP extent does not end on a sector boundary");
                break;
            }

            const uint64_t logical_sector = cursor / SECTOR_SIZE;
            const uint64_t physical_sector = extent->fe_physical / SECTOR_SIZE;
            const uint64_t sector_count = usable / SECTOR_SIZE;
            if (fprintf(out, "%" PRIu64 " %" PRIu64 " %" PRIu64 "\n",
                        logical_sector, physical_sector, sector_count) < 0) {
                result = fail_errno("write extent map");
                break;
            }

            cursor += usable;
            if ((extent->fe_flags & FIEMAP_EXTENT_LAST) != 0) saw_last = 1;
        }
        if (result != 0) break;
        if (cursor >= file_size) break;
        if (saw_last) {
            result = fail_msg("FIEMAP ended before covering the complete shadow file");
            break;
        }
    }

    if (result == 0 && fflush(out) != 0) result = fail_errno("flush extent map");
    if (result == 0 && fsync(fileno(out)) != 0) result = fail_errno("fsync extent map");
    if (fclose(out) != 0 && result == 0) result = fail_errno("close extent map");
    free(map);
    close(fd);

    if (result != 0) {
        unlink(output_path);
        return result;
    }

    return 0;
}
