//! GDT ディスクリプタと TSS の符号化（M4-a）。
//!
//! ビット配置を組み立てるだけの純粋ロジックであり、ホスト上の
//! `cargo test` で検証する。実際に `lgdt` / `ltr` を実行する部分は
//! [`super::table`] の責務。
//!
//! ロングモードでは、コード/データセグメントの base と limit は無視される
//! （フラットモデル固定）。それでもフィールドを正しく埋めるのは、無視される
//! のはあくまで**アドレス変換上**であって、ディスクリプタとしての妥当性検査
//! （P ビット、S ビット、型）は行われるため。

/// セグメントセレクタ。GDT 内のインデックスと要求特権レベルを組み合わせたもの。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentSelector(pub u16);

impl SegmentSelector {
    /// `index` 番目のディスクリプタを、特権レベル `rpl` で指すセレクタ。
    ///
    /// セレクタの下位 3 ビットは RPL(0:1) と TI(2、0 なら GDT) で、
    /// インデックスは 3 ビット左シフトした位置に入る。
    pub const fn new(index: u16, rpl: u16) -> Self {
        Self((index << 3) | (rpl & 0b11))
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn index(self) -> u16 {
        self.0 >> 3
    }

    pub const fn rpl(self) -> u16 {
        self.0 & 0b11
    }
}

/// アクセスバイトのビット。
pub mod access {
    /// Present。立っていないディスクリプタを使うと #NP になる。
    pub const PRESENT: u8 = 1 << 7;
    /// Descriptor type: 1 ならコード/データ、0 ならシステム（TSS 等）。
    pub const USER_SEGMENT: u8 = 1 << 4;
    /// Executable。立っていればコードセグメント。
    pub const EXECUTABLE: u8 = 1 << 3;
    /// コードセグメントでは Readable、データセグメントでは Writable。
    pub const READ_WRITE: u8 = 1 << 1;
    /// 使用可能な 64bit TSS を表すシステムディスクリプタ型。
    pub const TSS_AVAILABLE: u8 = 0x9;
    /// DPL（Descriptor Privilege Level）= 3。アクセスバイトの bit 6:5 に入る。
    /// ユーザー（Ring 3）用のコード/データディスクリプタで立てる（M5-e）。
    pub const DPL_RING3: u8 = 3 << 5;
}

/// フラグニブル（ディスクリプタ第 6 バイトの上位 4 ビット）。
pub mod flags {
    /// Granularity。limit を 4KiB 単位で解釈する。
    pub const GRANULARITY_4K: u8 = 1 << 3;
    /// Default operand size。64bit コードセグメントでは 0 でなければならない
    /// （`LONG_MODE` と同時に立てると不正）。
    pub const DEFAULT_OPERAND_32: u8 = 1 << 2;
    /// Long mode code segment。
    pub const LONG_MODE: u8 = 1 << 1;
}

/// カーネルコードセグメントのアクセスバイト。
pub const KERNEL_CODE_ACCESS: u8 =
    access::PRESENT | access::USER_SEGMENT | access::EXECUTABLE | access::READ_WRITE;
/// カーネルデータセグメントのアクセスバイト。
pub const KERNEL_DATA_ACCESS: u8 = access::PRESENT | access::USER_SEGMENT | access::READ_WRITE;
/// 64bit コードセグメントのフラグ。`DEFAULT_OPERAND_32` は立てない。
pub const KERNEL_CODE_FLAGS: u8 = flags::GRANULARITY_4K | flags::LONG_MODE;
/// データセグメントのフラグ。ロングモードでは実質無視されるが、
/// 32bit 互換の意味で妥当な値を入れておく。
pub const KERNEL_DATA_FLAGS: u8 = flags::GRANULARITY_4K | flags::DEFAULT_OPERAND_32;

/// ユーザー（Ring 3）64bit コードセグメントのアクセスバイト。カーネルコードと
/// 同じく実行可能・読み取り可能で、DPL=3 だけが異なる（M5-e）。
pub const USER_CODE_ACCESS: u8 = KERNEL_CODE_ACCESS | access::DPL_RING3;
/// ユーザー（Ring 3）データセグメントのアクセスバイト。カーネルデータと
/// DPL=3 だけが異なる。
pub const USER_DATA_ACCESS: u8 = KERNEL_DATA_ACCESS | access::DPL_RING3;
/// ユーザー 64bit コードセグメントのフラグ。カーネルコードと同じ
/// （`DEFAULT_OPERAND_32` は立てず `LONG_MODE`）。
pub const USER_CODE64_FLAGS: u8 = KERNEL_CODE_FLAGS;
/// ユーザー 32bit コードセグメントのフラグ。SYSRET の STAR 互換順を満たす
/// ためだけの枠で、M5-e/f では実際には使わない。32bit コードなので
/// `DEFAULT_OPERAND_32` を立て `LONG_MODE` は立てない。妥当なディスクリプタに
/// はする（`P`・コード・DPL=3）。
pub const USER_CODE32_FLAGS: u8 = flags::GRANULARITY_4K | flags::DEFAULT_OPERAND_32;
/// ユーザーデータセグメントのフラグ。カーネルデータと同じにする。**設計上の
/// 仮定にとどめず、稼働中の kdata の実バイトと D/B が一致することを起動時に
/// 読み戻しで確認する**（M5-e-1 の検証）。
pub const USER_DATA_FLAGS: u8 = KERNEL_DATA_FLAGS;

/// コード/データ用の 8 バイトディスクリプタを組み立てる。
///
/// ロングモードでは base/limit は無視されるため、base=0・limit=0xFFFFF
/// （4KiB 粒度で全空間）で固定する。
pub const fn user_segment_descriptor(access: u8, flags: u8) -> u64 {
    let limit: u64 = 0xF_FFFF;
    let base: u64 = 0;

    (limit & 0xFFFF)
        | ((base & 0xFF_FFFF) << 16)
        | ((access as u64) << 40)
        | (((limit >> 16) & 0xF) << 48)
        | (((flags & 0xF) as u64) << 52)
        | (((base >> 24) & 0xFF) << 56)
}

/// TSS 用の 16 バイトシステムディスクリプタを組み立て、`(下位, 上位)` で返す。
///
/// **ロングモードのシステムディスクリプタは 16 バイトである。** コード/データの
/// 8 バイトと同じつもりで扱うと、GDT 上の後続エントリがずれる。
pub const fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let low = ((limit as u64) & 0xFFFF)
        | ((base & 0xFF_FFFF) << 16)
        | (((access::PRESENT | access::TSS_AVAILABLE) as u64) << 40)
        | ((((limit as u64) >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let high = (base >> 32) & 0xFFFF_FFFF;
    (low, high)
}

/// x86_64 の TSS。
///
/// ロングモードの TSS はタスクスイッチには使われず、**スタックポインタの
/// 置き場**としてのみ機能する。
///
/// - `privilege_stack_table`（RSP0..RSP2）: 特権レベルが**下がる**方向の遷移
///   （ユーザーモード → カーネル）で使われる。ユーザーモードを導入する
///   M5 以降まで実際には効かない。
/// - `interrupt_stack_table`（IST1..IST7）: IDT エントリが IST を指定した
///   場合に、**特権レベルに関係なく**無条件で切り替わるスタック。ダブル
///   フォルトのように「現在のスタックが壊れている可能性がある」例外で使う。
///
/// フィールドが 8 バイト境界に載らないため `packed` が必要。
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct TaskStateSegment {
    reserved_0: u32,
    /// RSP0..RSP2。特権レベル遷移時に使う（M5 以降）。
    pub privilege_stack_table: [u64; 3],
    reserved_1: u64,
    /// IST1..IST7。IDT が指定したときに無条件で切り替わる。
    pub interrupt_stack_table: [u64; 7],
    reserved_2: u64,
    reserved_3: u16,
    /// I/O 許可ビットマップ（IOPB）への、TSS 先頭からのオフセット。
    ///
    /// **未設定（0）のままにしてはいけない。** 0 だと CPU は TSS の先頭を
    /// ビットマップとして解釈し、TSS のフィールドそのものを I/O 許可ビットと
    /// 読んでしまう。さらに 64KiB 分のビットマップを想定するため、TSS の
    /// 外側のメモリまで読みに行きうる。
    ///
    /// ここには **TSS の limit（= サイズ - 1）より大きい値**を入れる。
    /// Intel SDM の規定により、IOPB のオフセットが TSS の limit を超えて
    /// いる場合は「ビットマップ無し」と扱われ、CPL > IOPL の I/O アクセスは
    /// すべて #GP になる。[`TaskStateSegment::new`] はサイズそのもの（104）を
    /// 入れており、limit（103）より大きいのでこの条件を満たす。
    ///
    /// 実際に効いてくるのはユーザーモード（CPL 3）を導入する M5 以降だが、
    /// その時点で気づきにくい形で壊れるため、今のうちに正しく設定しておく。
    pub iomap_base: u16,
}

impl TaskStateSegment {
    pub const fn new() -> Self {
        Self {
            reserved_0: 0,
            privilege_stack_table: [0; 3],
            reserved_1: 0,
            interrupt_stack_table: [0; 7],
            reserved_2: 0,
            reserved_3: 0,
            // TSS の limit（サイズ - 1 = 103）より大きいので「IOPB 無し」に
            // なる。詳細は iomap_base のコメントを参照。
            iomap_base: core::mem::size_of::<Self>() as u16,
        }
    }
}

impl Default for TaskStateSegment {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selector_packs_the_index_above_the_rpl() {
        let selector = SegmentSelector::new(1, 0);
        assert_eq!(selector.bits(), 0x08);
        assert_eq!(selector.index(), 1);
        assert_eq!(selector.rpl(), 0);

        let user = SegmentSelector::new(4, 3);
        assert_eq!(user.bits(), (4 << 3) | 3);
        assert_eq!(user.index(), 4);
        assert_eq!(user.rpl(), 3);
    }

    #[test]
    fn the_kernel_code_descriptor_matches_the_well_known_value() {
        // 多くの実装で使われる 0x00AF9A000000FFFF と一致すること。
        let descriptor = user_segment_descriptor(KERNEL_CODE_ACCESS, KERNEL_CODE_FLAGS);
        assert_eq!(descriptor, 0x00AF_9A00_0000_FFFF);
    }

    #[test]
    fn the_kernel_data_descriptor_matches_the_well_known_value() {
        let descriptor = user_segment_descriptor(KERNEL_DATA_ACCESS, KERNEL_DATA_FLAGS);
        assert_eq!(descriptor, 0x00CF_9200_0000_FFFF);
    }

    #[test]
    fn the_user_descriptors_match_the_well_known_values() {
        // ユーザー用は、対応するカーネル用と DPL=3（アクセスバイトの 0x60）
        // だけが異なる。ucode32 は 32bit（フラグに D/B）。
        let ucode64 = user_segment_descriptor(USER_CODE_ACCESS, USER_CODE64_FLAGS);
        let ucode32 = user_segment_descriptor(USER_CODE_ACCESS, USER_CODE32_FLAGS);
        let udata = user_segment_descriptor(USER_DATA_ACCESS, USER_DATA_FLAGS);
        assert_eq!(ucode64, 0x00AF_FA00_0000_FFFF);
        assert_eq!(ucode32, 0x00CF_FA00_0000_FFFF);
        assert_eq!(udata, 0x00CF_F200_0000_FFFF);
    }

    #[test]
    fn the_user_access_bytes_set_dpl_three() {
        assert_eq!(USER_CODE_ACCESS & (0b11 << 5), access::DPL_RING3);
        assert_eq!(USER_DATA_ACCESS & (0b11 << 5), access::DPL_RING3);
        // DPL 以外はカーネル用と一致する。
        assert_eq!(USER_CODE_ACCESS & !(0b11 << 5), KERNEL_CODE_ACCESS);
        assert_eq!(USER_DATA_ACCESS & !(0b11 << 5), KERNEL_DATA_ACCESS);
    }

    #[test]
    fn the_user_code_flavours_differ_only_in_long_versus_operand_size() {
        // ucode64 は L=1 かつ D/B=0、ucode32 は L=0 かつ D/B=1。64bit コードで
        // L と D/B を同時に立てるのは不正（カーネルコードの既存テストの対）。
        assert_ne!(USER_CODE64_FLAGS & flags::LONG_MODE, 0);
        assert_eq!(USER_CODE64_FLAGS & flags::DEFAULT_OPERAND_32, 0);
        assert_eq!(USER_CODE32_FLAGS & flags::LONG_MODE, 0);
        assert_ne!(USER_CODE32_FLAGS & flags::DEFAULT_OPERAND_32, 0);
    }

    #[test]
    fn the_user_data_flags_match_the_kernel_data_flags() {
        // 設計上は kdata と同じ。実バイトの一致（D/B を含む）は起動時に
        // 読み戻しで確認するが、定数レベルでも揃っていることを固定する。
        assert_eq!(USER_DATA_FLAGS, KERNEL_DATA_FLAGS);
    }

    /// 64bit コードセグメントで L と D/B を同時に立てると不正になる。
    #[test]
    fn the_code_segment_does_not_set_both_long_mode_and_32bit_operand() {
        assert_ne!(KERNEL_CODE_FLAGS & flags::LONG_MODE, 0);
        assert_eq!(KERNEL_CODE_FLAGS & flags::DEFAULT_OPERAND_32, 0);
    }

    #[test]
    fn the_code_and_data_access_bytes_differ_only_in_the_executable_bit() {
        assert_eq!(
            KERNEL_CODE_ACCESS ^ KERNEL_DATA_ACCESS,
            access::EXECUTABLE,
            "コードとデータの違いは Executable ビットだけであるべき"
        );
    }

    #[test]
    fn a_tss_descriptor_spreads_the_base_across_both_halves() {
        let base = 0x0000_1234_5678_9ABC;
        let (low, high) = tss_descriptor(base, 103);

        // limit の下位 16 ビット。
        assert_eq!(low & 0xFFFF, 103);
        // base の下位 24 ビット。
        assert_eq!((low >> 16) & 0xFF_FFFF, base & 0xFF_FFFF);
        // base のビット 31:24。
        assert_eq!((low >> 56) & 0xFF, (base >> 24) & 0xFF);
        // base の上位 32 ビットは 2 個目の 8 バイトへ。
        assert_eq!(high, base >> 32);
    }

    #[test]
    fn a_tss_descriptor_is_present_and_marked_as_an_available_tss() {
        let (low, _) = tss_descriptor(0x1000, 103);
        let access_byte = ((low >> 40) & 0xFF) as u8;
        assert_ne!(access_byte & access::PRESENT, 0, "P ビットが必要");
        assert_eq!(
            access_byte & access::USER_SEGMENT,
            0,
            "システムディスクリプタなので S ビットは 0"
        );
        assert_eq!(access_byte & 0xF, access::TSS_AVAILABLE);
    }

    /// TSS のサイズと各フィールドのオフセットは仕様で決まっている。
    /// ここがずれると、CPU が別の場所をスタックポインタとして読む。
    #[test]
    fn the_tss_layout_matches_the_architecture_definition() {
        use core::mem::{offset_of, size_of};

        assert_eq!(size_of::<TaskStateSegment>(), 104);
        assert_eq!(offset_of!(TaskStateSegment, privilege_stack_table), 4);
        assert_eq!(offset_of!(TaskStateSegment, interrupt_stack_table), 36);
        assert_eq!(offset_of!(TaskStateSegment, iomap_base), 102);
    }

    /// IOPB のオフセットが TSS の limit を超えていること。超えていないと、
    /// CPU が TSS の内側または外側を I/O 許可ビットマップとして解釈する。
    /// GDT に入れる limit と同じ計算で確かめる。
    #[test]
    fn the_io_bitmap_offset_is_beyond_the_tss_limit() {
        let tss = TaskStateSegment::new();
        let iomap_base = tss.iomap_base as u32;
        let limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u32;

        assert_eq!(limit, 103);
        assert!(
            iomap_base > limit,
            "IOPB オフセット {iomap_base} は limit {limit} より大きくなければ \
             ならない（そうでないと I/O 許可ビットマップがあると解釈される）"
        );
        assert_ne!(iomap_base, 0, "0 だと TSS 先頭をビットマップとして読む");
    }

    #[test]
    fn a_new_tss_has_no_stacks_and_no_io_bitmap() {
        let tss = TaskStateSegment::new();
        // packed なフィールドへは参照を作れないため、いったんコピーする。
        let privilege_stacks = tss.privilege_stack_table;
        let interrupt_stacks = tss.interrupt_stack_table;
        let iomap_base = tss.iomap_base;
        assert_eq!(privilege_stacks, [0; 3]);
        assert_eq!(interrupt_stacks, [0; 7]);
        // iomap_base が TSS のサイズ以上なら「ビットマップ無し」。
        assert_eq!(
            iomap_base as usize,
            core::mem::size_of::<TaskStateSegment>()
        );
    }
}
