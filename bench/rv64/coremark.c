#include "coremark.h"

/* Exercise the unchanged upstream iterate() and all three algorithms. This is
 * a CRC-validated instruction mix, not a reportable CoreMark score. */
static ee_u8 memory[2000] __attribute__((aligned(16)));
volatile ee_s16 bench_seed1 = 0;
volatile ee_s16 bench_seed2 = 0;
volatile ee_s16 bench_seed3 = 0x66;

int main(void) {
    core_results result = {0};
    result.seed1 = bench_seed1;
    result.seed2 = bench_seed2;
    result.seed3 = bench_seed3;
    result.size = sizeof(memory) / NUM_ALGORITHMS;
    result.iterations = 1;
    result.execs = ALL_ALGORITHMS_MASK;
    result.memblock[0] = memory;
    for (unsigned i = 0; i < NUM_ALGORITHMS; ++i)
        result.memblock[i + 1] = memory + i * result.size;
    result.list = core_list_init(result.size, result.memblock[1], result.seed1);
    core_init_matrix(result.size, result.memblock[2],
                     (ee_s32)result.seed1 | ((ee_s32)result.seed2 << 16), &result.mat);
    core_init_state(result.size, result.seed1, result.memblock[3]);
    iterate(&result);
    return result.crclist != 0xe714 || result.crcmatrix != 0x1fd7
        || result.crcstate != 0x8e3a || result.err != 0;
}

void *memset(void *destination, int value, size_t count) {
    unsigned char *p = destination;
    for (size_t i = 0; i < count; ++i) p[i] = (unsigned char)value;
    return destination;
}
