#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "../uapi/loom_runtime.h"

static void test_layout(void)
{
    _Static_assert(sizeof(struct loom_rt_msg) == 48, "loom_rt_msg ABI drift");
    _Static_assert(sizeof(struct loom_rt_attr) == 8, "loom_rt_attr ABI drift");
    _Static_assert(LOOM_RT_MAX_MESSAGE == 65536U, "message cap changed");
    _Static_assert(LOOM_RT_ATTR_PRIVATE_BASE > LOOM_RT_KEY_ARGUMENT, "private key range overlaps core keys");
    _Static_assert(LOOM_RT_EVENT_CUSTOM_BASE > LOOM_RT_EVENT_MODULE_UNLOADED, "custom event range overlaps core events");
}

static void test_alignment(void)
{
    assert(loom_rt_attr_aligned_len(0) == 0);
    assert(loom_rt_attr_aligned_len(1) == 8);
    assert(loom_rt_attr_aligned_len(8) == 8);
    assert(loom_rt_attr_aligned_len(9) == 16);
}

static void test_repeated_typed_arguments(void)
{
    unsigned char buffer[128] = {0};
    struct loom_rt_msg *msg = (struct loom_rt_msg *)buffer;
    struct loom_rt_attr *a1;
    struct loom_rt_attr *a2;
    uint32_t value = 42;
    size_t off = sizeof(*msg);

    msg->magic = LOOM_RT_MAGIC;
    msg->abi_major = LOOM_RT_ABI_MAJOR;
    msg->abi_minor = LOOM_RT_ABI_MINOR;
    msg->opcode = LOOM_RT_OP_MODULE_CONTROL;
    msg->flags = LOOM_RT_F_REQUEST;
    msg->seq = 7;

    a1 = (struct loom_rt_attr *)(buffer + off);
    a1->key = LOOM_RT_KEY_ARGUMENT;
    a1->type = LOOM_RT_ATTR_STRING;
    a1->len = 4;
    memcpy(a1 + 1, "mode", 4);
    off += sizeof(*a1) + loom_rt_attr_aligned_len(a1->len);

    a2 = (struct loom_rt_attr *)(buffer + off);
    a2->key = LOOM_RT_KEY_ARGUMENT;
    a2->type = LOOM_RT_ATTR_U32;
    a2->len = sizeof(value);
    memcpy(a2 + 1, &value, sizeof(value));
    off += sizeof(*a2) + loom_rt_attr_aligned_len(a2->len);

    msg->payload_len = (uint32_t)(off - sizeof(*msg));

    assert(msg->payload_len == 32U);
    assert(a1->key == a2->key);
    assert(a1->type != a2->type);
    assert(memcmp(a1 + 1, "mode", 4) == 0);
    assert(memcmp(a2 + 1, &value, sizeof(value)) == 0);
}

int main(void)
{
    test_layout();
    test_alignment();
    test_repeated_typed_arguments();
    puts("loom runtime UAPI layout: PASS");
    return 0;
}
