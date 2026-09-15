OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
  headers PT_PHDR PHDRS ;
  text PT_LOAD FILEHDR ;
  data PT_LOAD ;
}

SECTIONS
{
  . = 0xffffffff80000000 + SIZEOF_HEADERS;

  .text : {
    *(.text._start)
    *(.text .text.*)
  } :text

  .rodata : {
    *(.rodata .rodata.*)
  } :text

  .data : {
    *(.data .data.*)
  } :data

  .bss : {
    *(.bss .bss.*)
    *(COMMON)
  } :data

  /DISCARD/ : {
    *(.eh_frame)
    *(.comment)
  }
}
