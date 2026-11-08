/* 崩溃隔离：把每一次跨边界调用包在一层 SEH 里。
 *
 * 为什么不用 Rust 的 catch_unwind：它只接 panic，接不住访问冲突/非法指令。
 * 插件是任意来源的原生代码，一段野指针就是整个启动器的死因。
 * 这个文件是整个"插件可以坏，宿主必须活"承诺的最后一道物理防线。
 *
 * 只能编译成 C（不带 /EHsc），并且不能被 LTCG 内联掉。
 */
#include <windows.h>

typedef int (*miy_guard_fn)(void *);

/* 与 Rust 侧 miy_abi::MIY_FAULT_SENTINEL 必须一致。 */
#define MIY_FAULT_SENTINEL 0x7A11

__declspec(noinline) int miy_guard(miy_guard_fn fn, void *arg)
{
    __try {
        return fn(arg);
    }
    __except (EXCEPTION_EXECUTE_HANDLER) {
        return MIY_FAULT_SENTINEL;
    }
}
