/* SPDX-License-Identifier: GPL-3.0-or-later */
#ifndef LOOM_RUNTIME_UAPI_H
#define LOOM_RUNTIME_UAPI_H

#ifdef __KERNEL__
#include <linux/types.h>
typedef __u8 loom_u8;
typedef __u16 loom_u16;
typedef __u32 loom_u32;
typedef __u64 loom_u64;
typedef __s32 loom_s32;
typedef __s64 loom_s64;
#else
#include <stdint.h>
typedef uint8_t loom_u8;
typedef uint16_t loom_u16;
typedef uint32_t loom_u32;
typedef uint64_t loom_u64;
typedef int32_t loom_s32;
typedef int64_t loom_s64;
#endif

#define LOOM_RT_MAGIC 0x4c4f4f4dU /* "LOOM" */
#define LOOM_RT_ABI_MAJOR 1U
#define LOOM_RT_ABI_MINOR 0U

#define LOOM_RT_MAX_MESSAGE (64U * 1024U)
#define LOOM_RT_ATTR_ALIGN 8U
#define LOOM_RT_ATTR_PRIVATE_BASE 0x1000U
#define LOOM_RT_EVENT_CUSTOM_BASE 0x8000U

/* Request opcodes. */
enum loom_rt_opcode {
    LOOM_RT_OP_INVALID = 0,
    LOOM_RT_OP_HELLO = 1,
    LOOM_RT_OP_GET_CAPS = 2,

    LOOM_RT_OP_MODULE_LOAD = 0x100,
    LOOM_RT_OP_MODULE_UNLOAD = 0x101,
    LOOM_RT_OP_MODULE_CONTROL = 0x102,
    LOOM_RT_OP_MODULE_LIST = 0x103,
    LOOM_RT_OP_MODULE_INFO = 0x104,

    LOOM_RT_OP_EVENT_PUBLISH = 0x200,
    LOOM_RT_OP_EVENT_POLL = 0x201,

    LOOM_RT_OP_HOOK_ACQUIRE = 0x300,
    LOOM_RT_OP_HOOK_RELEASE = 0x301,
    LOOM_RT_OP_HOOK_STATUS = 0x302,
    LOOM_RT_OP_HOOK_RECONCILE = 0x303,
};

/* Core lifecycle events. */
enum loom_rt_event {
    LOOM_RT_EVENT_INVALID = 0,
    LOOM_RT_EVENT_CORE_READY = 1,
    LOOM_RT_EVENT_POST_FS_DATA = 2,
    LOOM_RT_EVENT_FILESYSTEM_VIEW_READY = 3,
    LOOM_RT_EVENT_BOOT_COMPLETED = 4,
    LOOM_RT_EVENT_SUSPEND = 5,
    LOOM_RT_EVENT_RESUME = 6,
    LOOM_RT_EVENT_SHUTDOWN = 7,
    LOOM_RT_EVENT_MODULE_LOADED = 8,
    LOOM_RT_EVENT_MODULE_UNLOADED = 9,
};

/* Attribute value encoding. Attribute keys are message/opcode specific. */
enum loom_rt_attr_type {
    LOOM_RT_ATTR_INVALID = 0,
    LOOM_RT_ATTR_STRING = 1,
    LOOM_RT_ATTR_BYTES = 2,
    LOOM_RT_ATTR_U32 = 3,
    LOOM_RT_ATTR_U64 = 4,
    LOOM_RT_ATTR_S32 = 5,
    LOOM_RT_ATTR_S64 = 6,
    LOOM_RT_ATTR_BOOL = 7,
    LOOM_RT_ATTR_NESTED = 8,
};

/* Standard attribute keys shared by core opcodes. */
enum loom_rt_attr_key {
    LOOM_RT_KEY_INVALID = 0,
    LOOM_RT_KEY_MODULE_NAME = 1,
    LOOM_RT_KEY_MODULE_PATH = 2,
    LOOM_RT_KEY_MODULE_VERSION = 3,
    LOOM_RT_KEY_MODULE_DIGEST = 4,
    LOOM_RT_KEY_MODULE_PERSISTENCE = 5,
    LOOM_RT_KEY_EVENT_SOURCE = 6,
    LOOM_RT_KEY_ERROR_TEXT = 7,
    LOOM_RT_KEY_CAPABILITY = 8,
    LOOM_RT_KEY_HOOK_KIND = 9,
    LOOM_RT_KEY_HOOK_TARGET = 10,
    LOOM_RT_KEY_HOOK_PRIORITY = 11,
    LOOM_RT_KEY_HOOK_FLAGS = 12,

    /* Repeated typed parameters begin here. */
    LOOM_RT_KEY_ARGUMENT = 0x100,
};

/* Header flags. */
enum loom_rt_msg_flags {
    LOOM_RT_F_REQUEST = 1U << 0,
    LOOM_RT_F_REPLY = 1U << 1,
    LOOM_RT_F_EVENT = 1U << 2,
    LOOM_RT_F_MORE = 1U << 3,
    LOOM_RT_F_LEGACY = 1U << 4,
};

/*
 * Fixed 48-byte envelope. The payload immediately follows this structure and
 * is a sequence of aligned loom_rt_attr records.
 *
 * No user pointer may be retained by the kernel runtime after a request
 * returns. The full message must be copied into kernel-owned memory first.
 */
struct loom_rt_msg {
    loom_u32 magic;
    loom_u16 abi_major;
    loom_u16 abi_minor;
    loom_u16 opcode;
    loom_u16 flags;
    loom_u32 seq;
    loom_u32 module_handle;
    loom_u32 event_id;
    loom_u32 payload_len;
    loom_u32 reply_capacity;
    loom_s32 status;
    loom_u32 reserved[3];
};

/*
 * TLV header. `key` identifies the semantic parameter and `type` identifies
 * its representation. Multiple attributes with the same key are allowed,
 * which removes the historical single-string-argument limitation.
 */
struct loom_rt_attr {
    loom_u16 key;
    loom_u16 type;
    loom_u32 len;
};

static inline loom_u32 loom_rt_attr_aligned_len(loom_u32 len)
{
    return (len + (LOOM_RT_ATTR_ALIGN - 1U)) & ~(LOOM_RT_ATTR_ALIGN - 1U);
}

#endif /* LOOM_RUNTIME_UAPI_H */
