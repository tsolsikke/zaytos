/*
 * ZaytOS kernel リンカスクリプト。
 *
 * KERNEL_LOAD_ADDR は ADR-0009 で確定した低位固定ロードアドレス（LMA）。
 *
 * higher-half 移行では、リンクアドレス（VMA）を高位へ、ロードアドレス（LMA）を
 * 低位のまま据え置く。各セクションに AT(ADDR(section) - KERNEL_VIRT_BASE) を
 * 付けて VMA と LMA を分離する。現在 KERNEL_VIRT_BASE は 0 なので VMA == LMA で
 * あり、恒等リンクと同じ配置になる（B-2a-2 の追加は base=0 で振る舞い不変）。
 *
 * 低位ロードアドレス（0x100000 = 1MiB）を選んだ理由:
 *   - ExitBootServices 直後は UEFI が構築した既存のページテーブル
 *     （多くの場合、低位物理アドレスを恒等マッピングしている）が
 *     まだ有効であるため、bootloader が独自のページテーブルを
 *     用意しなくても kernel を配置できる。
 *   - x86_64-unknown-none の既定 code-model は "kernel"（高位負アドレス
 *     前提）だが、恒等リンクの間は "small" へ上書きしている
 *     （.cargo/config.toml 参照）。再リンク（B-2a-3）で "kernel" へ戻す。
 */

/* 再リンク後（B-2a-3）の入口はトランポリン。bootloader は p_paddr へ配置し、
 * elf.entry_point（トランポリンの VMA=高位）を LMA へ変換した低位アドレス
 * （KERNEL_LOAD_ADDR）へ飛ぶ。トランポリンが CR3 を静的初期テーブルへ切り替え、
 * 高位 _start へ jmp する。 */
ENTRY(zaytos_trampoline)

KERNEL_LOAD_ADDR = 0x100000;

/*
 * kernel イメージのリンクアドレスとロードアドレスの差（VMA - LMA）。
 *
 * higher-half 移行の前段として導入する。**現在は 0 である。**
 * 恒等マッピングなので、リンクアドレスとロードアドレスが一致している。
 *
 * B-2a-3 でここを 0xFFFFFFFF80000000 にした。
 * VMA = KERNEL_VIRT_BASE + KERNEL_LOAD_ADDR、LMA = KERNEL_LOAD_ADDR となる。
 * bootloader は p_paddr（LMA）へ配置し（B-2a-1）、トランポリンが静的初期
 * テーブルで高位へ移る（B-2a-2）。
 *
 * **この値は Rust 側にも同じものが必要になる。** 二重に持つと片方だけ
 * 直したときに食い違うので、build.rs がこのファイルから読み取って
 * Rust の定数を生成し、突き合わせる。
 */
KERNEL_VIRT_BASE = 0xFFFFFFFF80000000;

SECTIONS
{
    . = KERNEL_VIRT_BASE + KERNEL_LOAD_ADDR;

    /* kernel イメージ全体（bootloader が配置・zero-fill する範囲）の開始。
     * M2-d の物理フレームアロケータが、この範囲を「空き」として誤って
     * 配ってしまわないよう、kernel 自身がこのシンボルから自分の占有範囲を
     * 把握できるようにする（docs/architecture.md 申し送り参照）。 */
    __kernel_start = .;

    /* higher-half のトランポリン（B-2a）。イメージ先頭に置くことで、
     * 再リンク後に ENTRY をこれへ切り替えると、その LMA が KERNEL_LOAD_ADDR
     * になり、bootloader が低位 0x100000 へ飛べる。RIP 相対のみで、参照する
     * 静的テーブル・スタックは同じイメージ内にある。KEEP で GC から守る
     * （未参照セクションはリンカに捨てられるため）。 */
    .text.trampoline : AT(ADDR(.text.trampoline) - KERNEL_VIRT_BASE) ALIGN(4K) {
        KEEP(*(.text.trampoline))
    }

    .text : AT(ADDR(.text) - KERNEL_VIRT_BASE) ALIGN(4K) {
        *(.text .text.*)
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_VIRT_BASE) ALIGN(4K) {
        *(.rodata .rodata.*)
    }

    /* higher-half の静的初期ページテーブル（B-2a）。リンク時計算で全エントリを
     * 確定する。フレームアロケータより前、CR3 切り替えの瞬間に使う。KEEP で守る。 */
    .data.bootpt : AT(ADDR(.data.bootpt) - KERNEL_VIRT_BASE) ALIGN(4K) {
        KEEP(*(.data.bootpt))
    }

    /* higher-half のブートスタック（B-2a）。トランポリンが CR3 切り替え後に
     * RSP をここへ移し、UEFI/bootloader スタックの物理位置への依存を断つ。
     * 深さ実測のため 0xC5 で初期化する（.bss ではなく初期化データにする）。KEEP で守る。 */
    .data.bootstack : AT(ADDR(.data.bootstack) - KERNEL_VIRT_BASE) ALIGN(4K) {
        KEEP(*(.data.bootstack))
    }

    .data : AT(ADDR(.data) - KERNEL_VIRT_BASE) ALIGN(4K) {
        *(.data .data.*)
    }

    .bss : AT(ADDR(.bss) - KERNEL_VIRT_BASE) ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }

    __kernel_end = .;

    /* イメージ全体（物理）が静的初期テーブルの覆う範囲（恒等・高位とも
     * [0, 1GiB)）に収まること。収まらないと、トランポリンが高位 _start へ
     * 飛んだ瞬間、または恒等で低位ポインタを触った瞬間に未マップでフォルトする。
     * 物理イメージ末尾 = __kernel_end - KERNEL_VIRT_BASE。 */
    ASSERT(__kernel_end - KERNEL_VIRT_BASE <= 0x40000000,
           "kernel image must fit within the bootstrap tables' 1GiB coverage")

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note .note.*)
    }
}
