/* Let OVMF register its native IPv4/IPv6 boot options through SNP. */
#define EFI_DOWNGRADE_UX

/* Match the virtual-machine ROM's POST behaviour without delaying disk boot. */
#undef ROM_BANNER_TIMEOUT
#define ROM_BANNER_TIMEOUT 0
