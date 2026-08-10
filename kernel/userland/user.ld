/* ユーザープログラムのリンクスクリプト（S9-b-1）。
 *
 * 非 PIE の ET_EXEC を 0x400000 へリンクする。0x400000 は Linux の非 PIE の
 * 既定と同じで、「コストがゼロの範囲で Linux に寄せる」に当たる。
 *
 * .text.prepad を .text._start より前に置く。entry と最初の PT_LOAD の先頭を
 * 一致させないためである（理由は hello.rs のコメント）。 */

ENTRY(_start)

SECTIONS
{
  . = 0x400000;

  .text : {
    /* KEEP が要る。詰め物は entry から到達できないので、リンカのセクション
     * 回収（--gc-sections）が落とす。実際に落ちて entry が 0x400000 のままに
     * なった。 */
    KEEP(*(.text.prepad))
    *(.text._start)
    *(.text .text.*)
  }

  /* 各 PT_LOAD をページ境界から始める。
   *
   * 揃えないと、権限の違う 2 つの PT_LOAD が同じ 4KiB ページに載る。
   * 実測でそうなった（.text が 0x400000..0x400030、.rodata が 0x400030 から）。
   * 1 枚のページに 2 つの権限は載らないので、ローダーは後から来たほうを
   * 張れない。**実際のツールチェインも同じ理由でセグメントを揃える。** */
  . = ALIGN(4096);
  .rodata : { *(.rodata .rodata.*) }

  . = ALIGN(4096);
  .data   : { *(.data .data.*) }
  .bss    : { *(.bss .bss.*) *(COMMON) }

  /DISCARD/ : { *(.comment) *(.note.*) *(.eh_frame*) }
}
