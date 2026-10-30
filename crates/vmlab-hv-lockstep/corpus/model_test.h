/* SPDX-License-Identifier: MIT */
#ifndef VMLAB_MODEL_TEST_H
#define VMLAB_MODEL_TEST_H
#define RVMODEL_BOOT
#define RVMODEL_HALT \
    la t0, tohost; li t1, 1; sd t1, 0(t0); 991: j 991b
#define RVMODEL_IO_ASSERT_GPR_EQ(scratch, actual, expected) \
    li scratch, expected; beq scratch, actual, 992f; \
    la t0, tohost; li t1, 3; sd t1, 0(t0); 993: j 993b; 992:
#define RVMODEL_DATA_BEGIN \
    .pushsection .tohost,"aw",@progbits; .balign 8; \
    .global tohost; tohost: .dword 0; .popsection; \
    .balign 16; .global begin_signature; begin_signature:
#define RVMODEL_DATA_END \
    .balign 16; .global end_signature; end_signature:
#endif
