#!/bin/sh
# Rebuild vdso.so (embedded by the kernel with include_bytes!).
cd "$(dirname "$0")"
gcc -O2 -fPIC -shared -nostdlib -fno-stack-protector -fno-asynchronous-unwind-tables \
    -Wl,-soname,linux-vdso.so.1 -Wl,--hash-style=both -Wl,--version-script=vdso.lds \
    -Wl,-z,max-page-size=4096 -Wl,-T,layout.lds -Wl,--build-id=none -Wl,-z,noseparate-code -Wl,-z,norelro -o vdso.so vdso.c && strip vdso.so
