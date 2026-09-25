#pragma once
#define FE_TONEAREST 0
#define FE_DOWNWARD 0x400
#define FE_UPWARD 0x800
#define FE_TOWARDZERO 0xc00
static inline int fesetround(int r) { (void)r; return 0; }
static inline int fegetround(void) { return FE_TONEAREST; }
