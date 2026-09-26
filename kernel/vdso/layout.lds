/* One read+exec segment with file offsets equal to addresses, so the
   image can be copied into memory as-is (like Linux's vdso layout). */
SECTIONS {
    . = SIZEOF_HEADERS;
    .hash : { *(.hash) } :text
    .gnu.hash : { *(.gnu.hash) }
    .dynsym : { *(.dynsym) }
    .dynstr : { *(.dynstr) }
    .gnu.version : { *(.gnu.version) }
    .gnu.version_d : { *(.gnu.version_d) }
    .gnu.version_r : { *(.gnu.version_r) }
    .dynamic : { *(.dynamic) } :text :dynamic
    .rodata : { *(.rodata*) } :text
    .text : { *(.text*) }
    /DISCARD/ : { *(.data*) *(.bss*) *(.comment) *(.note*) *(.eh_frame*) }
}
PHDRS {
    text PT_LOAD FLAGS(5) FILEHDR PHDRS;
    dynamic PT_DYNAMIC FLAGS(4);
}
