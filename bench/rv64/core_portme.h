#ifndef VMLAB_CORE_PORTME_H
#define VMLAB_CORE_PORTME_H
#include <stddef.h>
#include <stdint.h>
#define HAS_FLOAT 0
#define HAS_TIME_H 0
#define USE_CLOCK 0
#define HAS_STDIO 0
#define HAS_PRINTF 0
#define COMPILER_VERSION "clang RV64GC"
#define COMPILER_FLAGS "-O2 -ffreestanding"
#define MEM_LOCATION "static RAM"
#define SEED_METHOD SEED_VOLATILE
#define MEM_METHOD MEM_STATIC
#define MULTITHREAD 1
#define MAIN_HAS_NOARGC 1
#define MAIN_HAS_NORETURN 0
#define PERFORMANCE_RUN 1
#define align_mem(x) ((void *)(((uintptr_t)(x) + 3) & ~(uintptr_t)3))
typedef int16_t ee_s16;
typedef uint16_t ee_u16;
typedef int32_t ee_s32;
typedef uint32_t ee_u32;
typedef uint8_t ee_u8;
typedef uintptr_t ee_ptr_int;
typedef size_t ee_size_t;
typedef uint64_t CORE_TICKS;
typedef struct { ee_u8 portable_id; } core_portable;
extern ee_u32 default_num_contexts;
void portable_init(core_portable *, int *, char *[]);
void portable_fini(core_portable *);
int ee_printf(const char *, ...);
#endif
