/* MIYLauncher 插件 ABI v1（修订 2）
 *
 * 用 C/C++/Zig/Go 写插件时，把这个头文件和 miy_abi 对齐即可。
 * 规则见 docs/architecture.md §4，最重要的一条：
 *   panic / C++ 异常 / 硬件异常都不得跨过这里的任何一个函数指针。
 *
 * 缓冲区约定：跨边界的堆内存一律是 [uint32 LE 载荷长度][载荷字节]，
 * 一律用 host->alloc 分配、用 host->free 释放，free 靠长度前缀还原布局。
 */
#pragma once
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

#define MIY_ABI_VERSION  1u
#define MIY_ABI_REVISION 2u

#define MIY_OK                 0
#define MIY_ERR_INVALID_ARG   -1
#define MIY_ERR_ABI           -2
#define MIY_ERR_UNKNOWN_SERVICE -3
#define MIY_ERR_PANIC         -4
#define MIY_ERR_REENTRANT     -5
#define MIY_ERR_TIMEOUT       -6
#define MIY_ERR_NOT_FOUND     -7
#define MIY_ERR_FAULT         -8

#define MIY_LOG_TRACE 0
#define MIY_LOG_DEBUG 1
#define MIY_LOG_INFO  2
#define MIY_LOG_WARN  3
#define MIY_LOG_ERROR 4

#define MIY_BUF_HDR 4u
#define MIY_BUF_MAX (16u * 1024u * 1024u)

#ifdef __cplusplus
extern "C" {
#endif

typedef struct MiyHostV1 MiyHostV1;

typedef void  (*miy_log_fn)(void* ctx, int32_t level, const uint8_t* msg, size_t msg_len);
typedef uint8_t* (*miy_alloc_fn)(void* ctx, size_t size);
typedef void  (*miy_free_fn)(void* ctx, uint8_t* ptr);
typedef uint8_t* (*miy_call_fn)(void* ctx, const uint8_t* service, size_t service_len,
                                const uint8_t* req, size_t req_len);
/* v1.2 追加：任务取消令牌。插件在循环里读 flag，宿主只置位不打断。
 * 用 32 位整数是为了 C/C++/Zig 侧可以直接当 volatile 整数读。 */
typedef struct MiyCancel { uint32_t flag; } MiyCancel;   /* 0 = 继续跑, 1 = 请停 */

typedef int32_t (*miy_task_entry)(void* arg, const MiyCancel* cancel);
typedef uint64_t (*miy_task_fn)(void* ctx, miy_task_entry entry, void* arg);
typedef void  (*miy_emit_fn)(void* ctx, const uint8_t* topic, size_t topic_len,
                             const uint8_t* payload, size_t payload_len);

struct MiyHostV1 {
    uint32_t abi_version;
    uint32_t struct_size;
    void* ctx;
    miy_log_fn   log;
    miy_alloc_fn alloc;
    miy_free_fn  free;
    miy_call_fn  call;
    miy_task_fn  task;
    miy_emit_fn  emit;
};

typedef int32_t (*miy_start_fn)(const MiyHostV1* host, void** out_state);
typedef void    (*miy_stop_fn)(void* state);

/* v1.1 追加：插件把 call("<plugin-id>.*") 落到自己身上的入口。
 * out 写入 [len][json] 缓冲区，必须用 host->alloc 分配。 */
typedef int32_t (*miy_dispatch_fn)(void* state,
                                   const uint8_t* service, size_t service_len,
                                   const uint8_t* req, size_t req_len,
                                   uint8_t** out);

typedef struct MiyPluginV1 {
    uint32_t abi_version;
    uint32_t struct_size;
    const char* id;
    const char* version;
    miy_start_fn start;
    miy_stop_fn  stop;
    miy_dispatch_fn dispatch;   /* v1.1 追加；旧插件此字段不存在，宿主靠 struct_size 判断 */
} MiyPluginV1;

/* 插件必须导出这个符号，返回一张静态的 MiyPluginV1。 */
extern const MiyPluginV1* miy_plugin_entry_v1(void);

#ifdef __cplusplus
}
#endif
