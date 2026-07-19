/*
 * ZaytOS kernel リンカスクリプト。
 *
 * KERNEL_LOAD_ADDR は暫定値（ADR-0009、確定は人間のレビュー待ち）。
 * 詳細な検討は ADR-0009 を参照。この時点では ELF ローダー（M2-0c）が
 * 未実装のため、ここでの値は「readelf/objdump でセクション配置を検証する
 * ための土台」であり、まだ実行はされない。
 *
 * 低位アドレス（0x100000 = 1MiB）を選んだ理由:
 *   - ExitBootServices 直後は UEFI が構築した既存のページテーブル
 *     （多くの場合、低位物理アドレスを恒等マッピングしている）が
 *     まだ有効であるため、ELF ローダーが独自のページテーブルを
 *     用意しなくても kernel へジャンプできる可能性が高い。
 *   - higher-half（例: 0xffffffff80000000 付近）を採用すると、
 *     M2-c で予定している「自前ページテーブル構築」の一部
 *     （最低限のマッピング）を M2-0c のローダー側に前倒しする
 *     必要が生じ、マイルストーンの境界が曖昧になる。
 *   - x86_64-unknown-none の既定 code-model は "kernel"（高位負アドレス
 *     前提）だが、低位アドレスに合わせて "small" へ上書きしている
 *     （.cargo/config.toml 参照）。
 */

ENTRY(_start)

KERNEL_LOAD_ADDR = 0x100000;

SECTIONS
{
    . = KERNEL_LOAD_ADDR;

    .text : ALIGN(4K) {
        *(.text .text.*)
    }

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K) {
        *(.data .data.*)
    }

    .bss : ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note .note.*)
    }
}
