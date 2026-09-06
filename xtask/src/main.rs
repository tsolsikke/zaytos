use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};

mod font;

const OVMF_CODE_PATH: &str = "/usr/share/OVMF/OVMF_CODE_4M.fd";
const OVMF_VARS_TEMPLATE_PATH: &str = "/usr/share/OVMF/OVMF_VARS_4M.fd";
const BOOTLOADER_PACKAGE: &str = "bootloader";
const UEFI_TARGET: &str = "x86_64-unknown-uefi";
const KERNEL_PACKAGE: &str = "kernel";
const KERNEL_TARGET: &str = "x86_64-unknown-none";
const PANIC_TEST_FEATURE: &str = "panic-test";
/// kernel 側の feature。有効にすると M3-a の描画テストパターンを描き、
/// コンソールを起動しない（ADR-0017）。
const GFX_TEST_PATTERN_FEATURE: &str = "gfx-test-pattern";

/// 例外ハンドラの回帰チェック（`--exception-test <kind>`）。
///
/// 起動完了後に意図した例外をわざと発生させ、ハンドラが呼ばれることを
/// 確認する。**必ず TCG で実行する。** KVM では `-d int` に何も残らず、
/// `v=..` の突き合わせができないため（troubleshooting.md 参照）。
struct ExceptionTest {
    /// `--exception-test` に渡す名前。
    name: &'static str,
    /// kernel 側で有効化する feature。
    feature: &'static str,
    /// 期待するベクタ番号。
    vector: u8,
    /// `qemu-debug.log` に現れる目印（`-d int` の出力形式）。
    qemu_marker: &'static str,
    /// シリアルログに現れることを追加で要求する文字列。
    extra_serial_markers: &'static [&'static str],
    /// 汎用レジスタの並び順を既知の値で突き合わせるか。
    check_registers: bool,
    /// `CPU Reset` の回数が起動時の 2 回から増えていないことを確認するか。
    /// ダブルフォルトがトリプルフォルトへ落ちていないことの確認に使う。
    check_no_extra_cpu_reset: bool,
}

/// `--exception-test invalid-opcode` が例外の直前に各 GPR へ入れる既知の値。
///
/// kernel 側の `known_register_values` と一致していなければならない。
/// スタブの push 順と `ExceptionContext` のフィールド順が食い違うと、
/// ダンプは出るのに名前と値の対応だけが入れ替わる。値がもっともらしいため
/// 気づきにくいので、レジスタごとに異なる値を入れて突き合わせる。
const KNOWN_REGISTERS: &[(&str, &str)] = &[
    ("rax", "0x1111111111111111"),
    ("rbx", "0x2222222222222222"),
    ("rcx", "0x3333333333333333"),
    ("rdx", "0x4444444444444444"),
    ("rsi", "0x5555555555555555"),
    ("rdi", "0x6666666666666666"),
    ("rbp", "0x7777777777777777"),
    ("r8", "0x8888888888888888"),
    ("r9", "0x9999999999999999"),
    ("r10", "0xaaaaaaaaaaaaaaaa"),
    ("r11", "0xbbbbbbbbbbbbbbbb"),
    ("r12", "0xcccccccccccccccc"),
    ("r13", "0xdddddddddddddddd"),
    ("r14", "0xeeeeeeeeeeeeeeee"),
    ("r15", "0xffffffffffffffff"),
];

const EXCEPTION_TESTS: &[ExceptionTest] = &[
    ExceptionTest {
        name: "divide-by-zero",
        feature: "exception-test-divide-by-zero",
        vector: 0,
        qemu_marker: "v=00",
        extra_serial_markers: &["error code = (none for this exception)"],
        check_registers: false,
        check_no_extra_cpu_reset: false,
    },
    ExceptionTest {
        name: "invalid-opcode",
        feature: "exception-test-invalid-opcode",
        vector: 6,
        qemu_marker: "v=06",
        extra_serial_markers: &[],
        check_registers: true,
        check_no_extra_cpu_reset: false,
    },
    ExceptionTest {
        name: "page-fault",
        feature: "exception-test-page-fault",
        vector: 14,
        qemu_marker: "v=0e",
        extra_serial_markers: &[
            // フォルトしたアドレスが CR2 から取れていること。
            "cr2=0x0000400000000000 (faulting address)",
            // エラーコードが人間に読める形へ展開されていること。
            "cause=page not present access=read mode=supervisor",
        ],
        check_registers: false,
        check_no_extra_cpu_reset: false,
    },
    ExceptionTest {
        name: "double-fault",
        feature: "exception-test-double-fault",
        vector: 8,
        qemu_marker: "v=08",
        extra_serial_markers: &[
            // IST1 へ切り替わっていること。切り替わっていなければ
            // 壊れている可能性のあるスタックの上でハンドラが動いている。
            "on IST1=true",
            // #DF のエラーコードは常に 0。
            "always zero for #DF",
        ],
        check_registers: false,
        // IST が効いていなければトリプルフォルトになり、CPU Reset が増える。
        check_no_extra_cpu_reset: true,
    },
];

/// 起動時に必ず記録される `CPU Reset` の回数（電源投入シーケンス、
/// `docs/troubleshooting.md` 参照）。これを超えたらリセットが起きている。
const EXPECTED_CPU_RESET_COUNT: usize = 2;

const EXCEPTION_TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// `zi` の台本を待つ猶予（H-b-2）。
///
/// # 共通の 20 秒では足りなくなった
///
/// **H-b-2 で台本に 3 つの回が増えた**——`/data/joined` の行の連結と、
/// **100 行のファイルを `cat` で 2 回撮る往復**である。
/// **実測で足りず、`/data/joined` の回の入口で QEMU を止めていた**
/// （**判定は 4 本とも「出力が無い」で落ちた**）。
///
/// **値は実測から決めた**（下の `(info)` の行が、毎回どれだけ掛かったかを出す）。
/// **実測は 16.1 秒である**——**20 秒のすぐ下で、通る回と落ちる回があった。**
/// **4 倍に近い余裕を取って 60 秒にする。** **台本を伸ばすときは、
/// その `(info)` の行を見て決め直すこと。**
///
/// **共通の定数を上げない。** **他の検査は 20 秒で足りており、
/// 上げるとハングの発見が全部遅くなる。**
///
/// **際限なく上げない。** **`--full` は `zi-test` を 19 回走らせる**ので、
/// **本当にハングしたときの待ち時間がそのまま 19 倍になる。**
const ZI_TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// 演習の行が出た後、判定が見る最後の行を待つ猶予（e-4 の手当て）。
///
/// **`cmd_virtio_irq_test` は 2 本の行を見ている**が、待っていたのは 1 本目
/// だけだった。**2 本目が出ないまま切ると、出ていないのか間に合わなかったのかが
/// 区別できない。** **破壊の構成では出ないことがある**ので、**待ち切りではなく
/// 猶予にしてある。**
const VIRTIO_IRQ_GRACE: Duration = Duration::from_secs(3);

/// クリティカルセクションとロックの回帰チェック（`--critical-test <kind>`）。
///
/// 検出の仕組みを入れても、それが機能しなければ意味がない。わざと異常経路を
/// 踏ませて、検出が働くことを確かめる（M4-b-1 のスタブ検証と同じ発想）。
struct CriticalTest {
    name: &'static str,
    feature: &'static str,
    /// シリアルログに現れることを要求する文字列。
    expected_markers: &'static [&'static str],
    /// 現れてはいけない文字列（検出をすり抜けたことを示すもの）。
    forbidden_markers: &'static [&'static str],
    /// 期待マーカーが出た時点で打ち切らず、必ずタイムアウトまで待つか。
    ///
    /// **「起きないこと」を確かめるテストで必要になる。** EOI を落とした
    /// ビルドはティック 1 回で `hlt` に入ったまま二度と起きない。カーネルは
    /// 失敗を報告することすらできない（報告するコードが動かない）ので、
    /// 「一定時間待ってもハートビートが 1 本も出ないこと」でしか判定できない。
    /// 早期に打ち切ると「まだ出ていないだけ」と区別がつかない。
    wait_for_full_timeout: bool,
    /// シリアルログに現れるべきハートビート行の最低数。
    ///
    /// `hlt` で眠ったまま起きなくなる「完全停止」は、カーネル内部からは
    /// 検出できない。外側からハートビートの本数を数えるのが唯一の手段。
    min_heartbeats: Option<usize>,
}

const CRITICAL_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "double-lock",
        feature: "critical-test-double-lock",
        expected_markers: &[
            "lock: double acquisition detected",
            "halting (cli + hlt loop)",
        ],
        // 検出をすり抜けて 2 回目の lock() が戻ってきた場合に出る行。
        forbidden_markers: &["double-lock detection FAILED"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // IF=1 の状態から InterruptGuard に入り、抜けたときに復元されることを
    // 確認する。IF=0 から入る経路は通常の起動ログで毎回通っているが、
    // 「保存値が IF=1 のときだけ sti する」という分岐の片側はここでしか
    // 通らない。一時的に sti するため、PIC を全マスクした M4-c-3 の後に
    // 実行する。
    CriticalTest {
        name: "restore-enabled",
        feature: "critical-test-restore-enabled",
        expected_markers: &[
            "critical-test: restore path OK",
            "critical-test: IF after sti = true",
            "critical-test: IF inside the guard = false",
            "critical-test: IF after the guard dropped = true",
        ],
        forbidden_markers: &["critical-test: restore path FAILED"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// 割り込みを有効化する経路の回帰チェック（`--interrupt-test <kind>`）。
///
/// 構造は [`CriticalTest`] と同じ。判定に必要なのは「出るべき行」と
/// 「出てはいけない行」の 2 つだけなので、型を分けずに使い回す。
const INTERRUPT_TESTS: &[CriticalTest] = &[
    // 全 IRQ をマスクしたまま sti し、何も届かないことを確認する。
    // 「回っているが割り込みが来ない」と「そもそも回っていない」を
    // 区別するため、周回回数が 0 でないことも見る。
    CriticalTest {
        name: "enable-only",
        feature: "interrupt-test-enable-only",
        expected_markers: &[
            "interrupt-test: sti-then-idle OK",
            "sti: interrupts are now enabled (IF=true",
            "heartbeat: loop iterations=",
            "sti-check summary: 4. interrupt delivery vectors are set as intended = UNVERIFIABLE",
        ],
        forbidden_markers: &[
            "sti-then-idle FAILED",
            "the loop never iterated",
            "an interrupt arrived while every IRQ is masked",
            "refusing to sti",
            "stack alignment:",
            "exception: vector=",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // int 0x40 をソフトウェア発行し、IRQ 経路が GPR を復元することを確認。
    CriticalTest {
        name: "irq-path",
        feature: "interrupt-test-irq-path",
        expected_markers: &[
            "irq-path: OK (the IRQ stub returned via iretq",
            "irq-path: int 0x40 handled (handler count for vector 0x40 = 1)",
        ],
        forbidden_markers: &[
            "irq-path: a general purpose register was not restored",
            "stack alignment:",
            "exception: vector=",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // タイマを実際に動かす（M4-d-2）。ティックが増え続けることが EOI の
    // 動作証明になる。
    CriticalTest {
        name: "timer",
        feature: "interrupt-test-timer",
        expected_markers: &[
            "interrupt-test: timer OK",
            // ICW2 の事後証明。M4-c-3 で未検証のまま残した論点が閉じる。
            // これは起動しきったことの目印でもある。
            KERNEL_BOOT_COMPLETE_MARKER,
            "pic: IMR after unmasking IRQ0 master=0xfe slave=0xff",
            "heartbeat: ticks=",
        ],
        // 500 ティックで止めるので 100 ティックごとのハートビートが 5 本。
        // 4 本以上出ていれば、ループは最後まで起き続けている。
        wait_for_full_timeout: false,
        min_heartbeats: Some(4),
        forbidden_markers: &[
            "interrupt-test: timer FAILED",
            "no tick arrived before the deadline",
            "the PIC vector offset (ICW2) is wrong",
            "configuring the PIT changed the interrupt mask",
            "stack alignment:",
            "exception: vector=",
        ],
    },
    // EOI をわざと落とし、ティックが 1 回で止まることを確認する。
    // **検証が実際に機能していることの確認**なので、期待する結果は失敗側。
    CriticalTest {
        name: "no-eoi",
        feature: "no-eoi-test",
        // **カーネルは失敗を報告できない。** EOI を送らないとティック 1 回で
        // `hlt` に入ったまま二度と起きず、判定コードに到達しないためである。
        // これは「無条件 hlt のメインループは完全停止を自己検出できない」と
        // いう既知の限界そのものであり、その限界を逆手に取った検証になる。
        // 期待するのは「最初のティックまでは確かに届いたこと」だけで、
        // その後止まったことは forbidden 側で外から見る。
        expected_markers: &[
            KERNEL_BOOT_COMPLETE_MARKER,
            "pit: channel 0 set to divisor=11932",
        ],
        // 2 回目以降が来ていればハートビートが出る。1 本でも出たら、
        // EOI 無しでもティックが続いたことになり検証が成立しない。
        forbidden_markers: &["interrupt-test: timer OK", "heartbeat: ticks="],
        // 「出ないこと」の確認なので早期に打ち切らず、最後まで待つ。
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // PIC をわざと 0x30-0x3F へ再マップし、ティックがそのベクタで届くことを
    // 確認する。**M4-c-3 で残した申し送りを閉じるためのテストである。**
    // 通常構成の 0x20 は OVMF が既に使っていた可能性が高く、こちらの ICW2 が
    // 誤っていても動いてしまいうる。別の値で届けばその疑いが晴れると同時に、
    // 配送元が LAPIC ではなく 8259A であることも示せる（LAPIC 経由なら
    // ベクタは移動しない）。
    CriticalTest {
        name: "alt-offset",
        feature: "alt-offset-test",
        expected_markers: &[
            "interrupt-test: timer OK",
            // ここが 0x20 のままなら、再マップが効いていないか LAPIC 由来。
            "timer: first tick arrived as vector 0x30",
            "pic: IMR after unmasking IRQ0 master=0xfe slave=0xff",
        ],
        forbidden_markers: &[
            "interrupt-test: timer FAILED",
            "no tick arrived before the deadline",
            "the PIC vector offset (ICW2) is wrong",
            "timer: first tick arrived as vector 0x20",
            "stack alignment:",
            "exception: vector=",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: Some(4),
    },
    // IRQ スタブ表の索引を 1 本ずらし、配置検証が働くことを確認する。
    //
    // **この検査は S6-a まで「落ちるところを一度も見ていない」側だった。**
    // 既存の破壊 38 件のいずれもこの検査を落としていないことを確かめたうえで
    // 置いた（`verification-coverage.md`）。
    //
    // **ずらすのはベクタ `0x3F` の 1 本だけである。** 既定ビルドでそこへ
    // 割り込みは届かないので、**振る舞いは変わらず検査だけが落ちる。**
    // 落ちた結果として ADR-0018 §2 の門が `sti` を拒む。
    CriticalTest {
        name: "irq-stub-offset",
        feature: "idt-irq-stub-offset-test",
        expected_markers: &[
            "irq stub table ok=false",
            "sti-check summary: 3. IDT loaded, all exception gates present = FAILED",
            "the pre-sti checks did not pass; refusing to sti",
        ],
        forbidden_markers: &["irq stub table ok=true", "sti: interrupts are now enabled"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 境界調整をわざと外し、境界検証が働くことを確認する。
    // **検証が壊れていないことを確かめるためのテストなので、期待する結果は
    // 「検出して停止する」である。**
    CriticalTest {
        name: "misaligned",
        feature: "misalign-test,interrupt-test-irq-path",
        expected_markers: &[
            "stack alignment:",
            "violated the SysV ABI",
            "halting (cli + hlt loop)",
        ],
        forbidden_markers: &["irq-path: OK"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// ページテーブルの分割・アンマップの回帰チェック（`--paging-test <kind>`）。
///
/// いずれも「検査が実際に働くこと」を確かめる。正常系は通常起動の
/// `split-test:` 行が毎回見ているので、ここには置かない。
const PAGING_TESTS: &[CriticalTest] = &[
    // S7-c: 新しいアドレス空間へカーネルの上位を写さずに CR3 を差し替える。
    //
    // **到達条件4（CR3 切り替え後もカーネルが動くこと）の対照である。** 既定ビルドでは
    // 切り替えた後の行が出る。**上位を写さないと、切り替えた瞬間に命令フェッチが
    // 翻訳できなくなり、その行が出ない。**
    //
    // **「動いた」を主張する検査には、動かない側が要る。** 既定ビルドの行だけでは、
    // 切り替えが実際に効いているのか何もしていないのかを区別できない。
    CriticalTest {
        name: "addrspace-no-kernel-share",
        feature: "addrspace-no-kernel-share",
        expected_markers: &["address-space: built a second address space"],
        forbidden_markers: &["address-space: still running after the switch"],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // S9-a: map_4kib の書き込み可否の引数を無視し、葉を常に W=1 で作る。
    //
    // **既定ビルドの主張は「writable=false で張った葉に Ring 3 が書くと #PF になる」**
    // で、ring3-vectors の 6 本目（#PF-write-ro）がそれを見ている。この破壊は
    // W=0 を作らせないので、Ring 3 の書きが通り、命令列の末尾に置いた ud2 が
    // ベクタ 6 で畳まれる。判定行が「ベクタが違う」と言って止まる。
    //
    // **feature 名は paging-test の傘に入れていない。** 傘に入れると Ring 3 の
    // 検証自体が載らず、破壊を観測する側が消える。
    CriticalTest {
        name: "map-force-writable",
        feature: "map-force-writable",
        expected_markers: &["#PF-write-ro folded with vector=6", "halting"],
        forbidden_markers: &["ring3-vectors: all six Ring 3 faults"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 正しい実装で、PCD 付きの 2MiB ページを分割しても属性が残ること。
    // わざと壊す側（drop-pcd）と対にして初めて意味を持つ。
    CriticalTest {
        name: "pcd",
        feature: "paging-test",
        expected_markers: &[
            "paging-test: scratch",
            "paging-test: PCD survived the split on all 512 entries = OK",
            "paging-test: done",
        ],
        forbidden_markers: &["PCD was lost", "exception: vector="],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 分割時に PCD を落とす。読み戻し照合が不一致を検出すること。
    CriticalTest {
        name: "drop-pcd",
        feature: "paging-test-drop-pcd",
        expected_markers: &["paging-test: PCD was lost on 512 of 512 entries after the split = NG"],
        forbidden_markers: &["PCD survived the split"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // PD エントリの差し替えを先に行い、中間状態を意図的に踏む。
    // **#PF が起きること**と、CR2 が踏んだアドレスを指すことを見る。
    CriticalTest {
        name: "wrong-order",
        feature: "paging-test-wrong-order",
        expected_markers: &["exception: vector=14", "halting (cli + hlt loop)"],
        forbidden_markers: &["split-test: split and unmap behave as planned"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // アンマップの添字を間違える。対象が生きたまま別のページが消えること。
    CriticalTest {
        name: "bad-index",
        feature: "paging-test-bad-index",
        expected_markers: &["split-test: the split/unmap round trip did not behave as planned"],
        forbidden_markers: &["split-test: split and unmap behave as planned"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // アンマップしたページを読む。正しい実装では #PF（vector=14）になる。
    CriticalTest {
        name: "unmap-fault",
        feature: "paging-test-unmap-fault",
        expected_markers: &[
            "paging-test: about to read the unmapped page",
            "exception: vector=14",
            "halting (cli + hlt loop)",
        ],
        forbidden_markers: &["the read did NOT fault"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // invlpg を落とす。**アンマップ前に触ってあるので TLB にエントリが
    // ある。** それが残っているとフォルトせずに古い値が読める。
    // unmap-fault と対にして初めて「invlpg が効いている」と言える。
    //
    // 対象ページをアンマップ前に触らないと、そもそも TLB エントリが存在せず
    // invlpg の有無が結果に現れない。最初にそう書いてしまい、両方とも #PF に
    // なって差が出なかった。
    CriticalTest {
        name: "no-invlpg",
        feature: "paging-test-no-invlpg",
        expected_markers: &[
            "paging-test: about to read the unmapped page",
            "paging-test: the read did NOT fault",
            "paging-test: done",
        ],
        forbidden_markers: &["exception: vector=14"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 稼働中のヒープが載る 2MiB ページを、使いながら分割する。
    CriticalTest {
        name: "split-heap",
        feature: "paging-test-split-heap",
        expected_markers: &["paging-test: split the live heap page", "paging-test: done"],
        forbidden_markers: &["exception: vector="],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // A-1 の direct map 窓を高位ではなく低位で張る。独立 walker が高位窓の
    // 不在を検出し、CR3 を切り替えずに止まること。壊れていなければ
    // `direct-map: verified` まで進むので、それを forbidden にして対にする。
    CriticalTest {
        name: "directmap-low-window",
        feature: "paging-test-directmap-low-window",
        expected_markers: &["does not resolve: NotPresent", "refusing to switch CR3"],
        forbidden_markers: &[
            "direct-map: verified",
            "direct-map: CR3 switch instruction executed",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // A-2 の登録窓の base を 1 ページずらす。差し替え直後の phys_to_virt 検証が
    // 食い違いを検出し、フレームバッファ・コンソールの高位アクセスへ進む前に
    // 止まること。壊れていなければ登録窓が有効になるので、それを forbidden に
    // して対にする。
    CriticalTest {
        name: "directmap-wrong-base",
        feature: "paging-test-directmap-wrong-base",
        expected_markers: &["phys_to_virt", "the registered window is wrong"],
        forbidden_markers: &["direct-map A-2: registered window active"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// カーネルスタックのガードページの回帰チェック（`--stack-test <kind>`、M5-b）。
const STACK_TESTS: &[CriticalTest] = &[
    // スタックを溢れさせ、ガードページに触れた #PF が IST2 上で、CR2 =
    // ガードページとして報告されること。#DF へ昇格しないこと。
    CriticalTest {
        name: "guard",
        feature: "stack-guard-test",
        expected_markers: &[
            "exception: vector=14 (#PF",
            "cr2 is in the kernel stack guard page = true",
            "on IST2=true",
        ],
        forbidden_markers: &["exception: vector=8"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **張る前のガードページを踏む（ADR-0046 の Addendum）。**
    //
    // **ガードページは張った後しか効かない。** **張る前に溢れても黙って通る**
    // ——実際に踏んだ（ADR-0046 の実装。**気づいたのは `old pte` の A/D で、
    // あれは偶然映っていただけである**）。**この破壊は、その形へわざと戻す。**
    //
    // **`stack-guard`（上）とは別の面である**——**あちらは張った後に触れた
    // #PF を見る。こちらは張る前に触れていたことを見る。**
    CriticalTest {
        name: "untouched",
        feature: "stack-overflow-before-guard-test",
        expected_markers: &[
            "stack-guard: the kernel stack guard page was untouched before it was installed = \
             false",
        ],
        forbidden_markers: &[
            "stack-guard: the kernel stack guard page was untouched before it was installed = true",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // #PF に IST を与えず、溢れ → 壊れたスタック上の #PF → #DF の本来の連鎖で
    // ダブルフォルトが出ること。#PF が IST2 上で完結しないこと。
    CriticalTest {
        name: "df",
        feature: "stack-overflow-df-test",
        expected_markers: &["exception: vector=8 (#DF", "on IST1=true"],
        forbidden_markers: &["on IST2=true"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// 協調的コンテキストスイッチの回帰チェック（`--task-test <kind>`、M5-c）。
const TASK_TESTS: &[CriticalTest] = &[
    // スイッチで次タスクの rbx を壊す。復帰したタスクが GPR 照合で検出する。
    CriticalTest {
        name: "drop-reg",
        feature: "task-switch-drop-reg",
        expected_markers: &["GPR(s) corrupted across the switch", "halting"],
        forbidden_markers: &["cooperative switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // RSP の差し替えを省く。スイッチが起きず、ワーカーが走らないまま会計が
    // 合わないことを検出する。
    CriticalTest {
        name: "no-swap",
        feature: "task-switch-no-swap",
        expected_markers: &["accounting did not balance"],
        forbidden_markers: &["cooperative switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // InterruptGuard 保持中に yield を呼ぶ。on_yield のガードが fail-fast する。
    CriticalTest {
        name: "in-critical",
        feature: "task-switch-yield-in-critical",
        expected_markers: &[
            "yield called while holding a Locked/InterruptGuard",
            "halting",
        ],
        forbidden_markers: &["cooperative switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // プリエンプティブスイッチで RSP0 の更新を落とす。M5-c の RSP0 読み戻し
    // 検査が食い違いを検出して halt する。
    CriticalTest {
        name: "drop-rsp0",
        feature: "task-switch-drop-rsp0",
        expected_markers: &["TSS.RSP0 readback", "halting"],
        forbidden_markers: &["preemptive switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // プリエンプト窓を広げると窓カウントが増えること（統計的検証の判定が働く
    // ことの裏）。窓カウント > 0 で verified まで進めば OK。窓が広い分だけ
    // カウントは normal より増える（値は非決定的なので verified の到達で見る）。
    CriticalTest {
        name: "widen-window",
        feature: "task-widen-preempt-window",
        expected_markers: &["preempts in the GPR window=", "preemptive switch verified"],
        forbidden_markers: &["no preemption landed in the GPR window"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // InterruptGuard の cli を落とし、防御スキップも外す。Locked 保持中に timer が
    // プリエンプトして別ワーカーが同じ Locked を取り、二重取得検出が発火する。
    CriticalTest {
        name: "preempt-in-critical",
        feature: "task-preempt-in-critical",
        expected_markers: &["double acquisition detected", "halting"],
        forbidden_markers: &["preemptive switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// Ring 3 遷移の破壊確認（M5-e-4）。いずれも遠征が verified に到達しないことを
/// 確かめる。
const RING3_TESTS: &[CriticalTest] = &[
    // ucode64 の DPL を 0 にする。M5-e-1 の読み戻しアサートは cfg で外してあり、
    // iretq 自身が #GP になる（Ring 3 に落ちない。フォルト元 Ring 0 なので畳まれない）。
    CriticalTest {
        name: "user-desc-dpl0",
        feature: "ring3-test-user-desc-dpl0",
        expected_markers: &["exception: vector=13", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // ユーザーページの USER を落とす。遠征前の両側 U/S 監査が捕まえる。
    CriticalTest {
        name: "user-page-supervisor",
        feature: "ring3-test-user-page-supervisor",
        expected_markers: &["U/S audit before the excursion failed", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 遠征の RSP0 据え付けを落とす。#GP がメインのスタックで走る。
    //
    // S8-c で捕まえる場所が変わった。フレームの健全性判定の従（ハンドラ自身が
    // そのベクタの想定スタックにいること）が先に落ちるので、畳まずに dump+halt する。
    // 以前は畳んだ後に遠征の呼び出し側が handler_in_excursion で捕まえていた
    // （"RSP0 did not take effect"）。**この破壊は健全性判定の従を実証する側でもある。**
    CriticalTest {
        name: "drop-rsp0",
        feature: "ring3-test-drop-rsp0",
        expected_markers: &["exception frame is not trustworthy", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 遠征フラグを立てない。cli の #GP が畳まれず dump+halt する（フォルト RIP が
    // ユーザーコード入口なのが user-desc-dpl0 との違い）。
    CriticalTest {
        name: "no-fold-flag",
        feature: "ring3-test-no-fold-flag",
        expected_markers: &["exception: vector=13", "rip=0x0000008000000000", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 例外フレームの CS を既知でない値へ差し替える。畳みの3条件は通り、健全性判定の
    // 主（CS が既知のセレクタであること）だけが落ちて dump+halt する。
    // 畳めるはずの Ring 3 の #GP に掛けてあるので、止まったのが健全性判定のためだと
    // 特定できる（もともと畳まない例外に掛けると区別が付かない）。
    CriticalTest {
        name: "corrupt-frame-cs",
        feature: "ring3-test-corrupt-frame-cs",
        expected_markers: &["exception frame is not trustworthy", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S9-b-1: 埋め込んだユーザープログラムの破壊確認。
    CriticalTest {
        name: "user-skip-load",
        feature: "user-run-skip-load",
        // ゼロを実行して落ちる。**畳んだ位置とベクタは主張しない**——ゼロは
        // `add [rax], al` なので、どこで落ちるかは入場時の RAX に依る。
        expected_markers: &["user-run: hello folded instead of exiting", "halting"],
        forbidden_markers: &["user-load: hello ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **この破壊だけが user-load の読み戻しまで到達する。** map-force-writable は
    // 先に ring3-vectors の 6 本目が落ちるので、後ろの読み戻しへ届かない。
    CriticalTest {
        name: "user-writable-text",
        feature: "user-run-writable-text",
        expected_markers: &["has w=true (expected false)", "halting"],
        forbidden_markers: &["user-load: hello ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "user-wrong-entry",
        feature: "user-run-wrong-entry",
        expected_markers: &[
            "user-run: hello folded instead of exiting",
            "rip=0x400000",
            "halting",
        ],
        forbidden_markers: &["user-load: hello ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S9-b-3-1: プロセスの終了の破壊確認。
    //
    // exit を受けても終了させない。**Ring 3 へ返り、直後の ud2 で畳まれる。**
    // 受け皿を破壊と一緒に用意してあるので、行き先は確定している（entry + 0x30）。
    CriticalTest {
        name: "user-exit-ignored",
        feature: "user-exit-ignored",
        expected_markers: &[
            "user-run: hello folded instead of exiting (vector=6",
            "rip=0x400040",
            "halting",
        ],
        forbidden_markers: &["user-load: hello ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // BKL を保持したまま longjmp する。**次に取る者が同じコアの再取得として捕まえる。**
    // 取る入口に「出口を通らない経路」ができたことそのものの反証である。
    CriticalTest {
        name: "user-exit-keep-bkl",
        feature: "user-exit-keep-bkl",
        expected_markers: &[
            "bkl: recursive acquisition",
            "at entry SteadyLoop",
            "halting",
        ],
        forbidden_markers: &["user-load: hello ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 終了しても空間を畳まない。**会計が合わなくなる。**
    CriticalTest {
        name: "user-exit-keep-space",
        feature: "user-exit-keep-space",
        expected_markers: &["left the allocator short", "halting"],
        forbidden_markers: &["user-load: hello ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 終了状態を RDI でなく RSI から読む。**終了状態の一致も、判定行が主張して
    // いる道の 1 つである。** 記録された値は主張しない（`hello` の `.rodata` の
    // 番地に依る）。**主張するのは「0 でない値が入り、判定が落ちること」までである。**
    CriticalTest {
        name: "user-exit-wrong-status",
        feature: "user-exit-wrong-status",
        expected_markers: &[
            "user-run: hello exited with status",
            "expected 0",
            "halting",
        ],
        forbidden_markers: &["user-load: hello ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// int 0x80 システムコールの破壊確認（M5-f-1-2）。いずれも probe の往復が verified に
/// 到達しないことを確かめる。
const SYSCALL_TESTS: &[CriticalTest] = &[
    // 第 4 引数を context.r10 でなく context.rcx から読む。probe が記録した第 4 引数が
    // 期待値と食い違い、検証が argument register mismatch で止まる（R10 規約の実証）。
    CriticalTest {
        name: "arg4-rcx",
        feature: "syscall-test-arg4-rcx",
        expected_markers: &["syscall: argument register mismatch", "halting"],
        forbidden_markers: &["syscall: probe int 0x80 round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // ゲート 0x80 を DPL=0 にする。Ring 3 からの int 0x80 がゲート DPL<CPL で #GP になり、
    // syscall_entry に到達しない。
    //
    // S8-a で捕まえる場所が変わった。畳みの判定から RIP の厳密一致を外したので、この
    // #GP は int の位置で畳まれてカーネルへ戻る。予期は cli の位置なので、遠征の
    // 呼び出し側の主張（assert_folded_at）が食い違いを捕まえて停止する。
    // 以前は畳まれずに例外ダンプへ落ちていた（"exception: vector=13"）。
    CriticalTest {
        name: "gate-dpl0",
        feature: "syscall-test-gate-dpl0",
        expected_markers: &["folded at an unexpected place", "halting"],
        forbidden_markers: &["syscall: probe int 0x80 round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 戻り値の context.rax 書き戻しを落とす。ユーザーが store した値が PROBE_RETURN と
    // 食い違い、検証が return value mismatch で止まる。
    CriticalTest {
        name: "drop-retval",
        feature: "syscall-test-drop-retval",
        expected_markers: &["syscall: return value mismatch", "halting"],
        forbidden_markers: &["syscall: probe int 0x80 round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // ユーザーポインタ検証（M5-f-2-1）。葉/中間の U=1 判定を外す。無効3（supervisor in
    // user range）が受理され、battery が検出して halt する。
    CriticalTest {
        name: "validate-skip-us",
        feature: "syscall-test-validate-skip-us",
        expected_markers: &[
            "syscall: pointer validation battery failed",
            "supervisor in user range",
            "halting",
        ],
        forbidden_markers: &["syscall: pointer validation battery verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // ページ走査を先頭ページだけで打ち切る。無効4（straddle）の末尾無効を取り逃して
    // 受理され、battery が検出して halt する。
    CriticalTest {
        name: "validate-skip-laststep",
        feature: "syscall-test-validate-skip-laststep",
        expected_markers: &[
            "syscall: pointer validation battery failed",
            "straddle last page",
            "halting",
        ],
        forbidden_markers: &["syscall: pointer validation battery verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 検証器を常に受理にする。最初の拒否ケース（kernel pointer）が受理され、battery が
    // 検出して halt する（多層防御の最後の砦の確認）。
    CriticalTest {
        name: "validate-skip-all",
        feature: "syscall-test-validate-skip-all",
        expected_markers: &[
            "syscall: pointer validation battery failed",
            "kernel pointer",
            "halting",
        ],
        forbidden_markers: &["syscall: pointer validation battery verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // copy_from_user が検証を経ずに読む（M5-f-2-2）。カーネルポインタで -EFAULT のはずが総和が
    // 返り、内容往復の検証が「-EFAULT のはずが値」を検出して halt する。
    CriticalTest {
        name: "einval-as-efault",
        feature: "syscall-test-einval-as-efault",
        expected_markers: &[
            "syscall: checksum case 'over-long' expected -EINVAL",
            "halting",
        ],
        forbidden_markers: &["syscall: checksum round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S10-b: ディレクトリを read したときの errno を取り違える。EISDIR と ENOTDIR は
    // どちらも「種別が違う」を意味するので雑に見ると同じに見えるが、Linux は分けている。
    // syscall-test の 16 番目の検算が食い違いを捕まえ、終了状態 16 で止まる。
    CriticalTest {
        name: "eisdir-as-enotdir",
        feature: "syscall-test-eisdir-as-enotdir",
        expected_markers: &[
            "user-run: syscall-test exited with status 16",
            "reading a directory did not return -EISDIR",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S10-b: read が位置を進めない。**1 回だけ読むぶんには正しく見える**ので、
    // 最初に落ちるのは「末尾での read が 0 を返す」の検算である（位置が 0 のままなので
    // 2 回目も 18 を返す）。**短く読んでから続きを読む検算より先に、ここが捕まえる。**
    CriticalTest {
        name: "read-no-advance",
        feature: "syscall-test-read-no-advance",
        expected_markers: &[
            "user-run: syscall-test exited with status 13",
            "reading at the end of the file did not return 0",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S10-b: stat の st_blocks を 512 バイト単位でなくバイト数で書く。**単位の
    // 取り違えは値がもっともらしいままである**（4096 は 8 と同じくらい「ありそう」に
    // 見える）ので、突き合わせる相手が無いと気づけない。
    CriticalTest {
        name: "stat-blocks-in-bytes",
        feature: "syscall-test-stat-blocks-in-bytes",
        expected_markers: &[
            "user-run: syscall-test exited with status 21",
            "st_blocks was not 8 (512-byte units)",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S10-b: getdents64 の d_reclen を 8 バイト境界へ切り上げない。**こちらの走査は
    // d_reclen を頼りに歩くので、外しても自分では気づけない。** 整列は呼び出し側との
    // 約束なので、約束を見ている検算だけが捕まえる。
    CriticalTest {
        name: "dirent-no-align",
        feature: "syscall-test-dirent-no-align",
        expected_markers: &[
            "user-run: syscall-test exited with status 26",
            "a d_reclen was not a multiple of 8",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S11-1: 初期スタックの auxv の終端を書かない。**自作のプログラムは auxv を
    // 読まない**ので、足した項目も終端の欠落も、歩かなければ分からない。
    CriticalTest {
        name: "no-auxv-terminator",
        feature: "syscall-test-no-auxv-terminator",
        expected_markers: &[
            "user-run: syscall-test exited with status 35",
            "the auxv terminator (AT_NULL) was missing",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S11-5: 深さの上限で断ったことを -EAGAIN でなく -ENOSYS で返す。**どちらも
    // 「できない」を意味するので、雑に見ると同じに見える**（S10-b の 4 つと同じ族）。
    // **-ENOSYS は「その番号は無い」、-EAGAIN は「その番号は在るが今は受け付け
    // られない」である。** 上限が効いていることを主張しているのは後者だけで、
    // `spawn-test` が孫の側からその差を突く。
    CriticalTest {
        name: "spawn-eagain-as-enosys",
        feature: "spawn-eagain-as-enosys",
        expected_markers: &[
            "user-run: syscall-test exited with status 40",
            "the grandchild was not refused",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // zi-c: 書きで開いた fd への write が複製へ足さない。**検証も戻り値も
    // 正しい**——読み戻し（58 番。長さ・バイト列・EOF）だけが捕まえる。
    CriticalTest {
        name: "write-file-skip-append",
        feature: "write-file-skip-append-test",
        expected_markers: &["user-run: syscall-test exited with status 58"],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // zi-c: 別の inode へ足す。的が空のまま残り、読み戻し（58 番）が捕まえる。
    // カナリア（60 番）まで届かない——58 番が先に落ちる。
    CriticalTest {
        name: "write-file-wrong-inode",
        feature: "write-file-wrong-inode-test",
        expected_markers: &["user-run: syscall-test exited with status 58"],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // zi-c: O_TRUNC の切り詰めを落とす。古い中身が先頭に残り、読み戻しの
    // バイト列の突き合わせ（58 番）が捕まえる。
    CriticalTest {
        name: "open-skip-truncate",
        feature: "open-skip-truncate-test",
        expected_markers: &["user-run: syscall-test exited with status 58"],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S11-5: 入れ子の遠征から戻ったとき、親の記録を戻さない。**子の write と
    // 終了状態が、親のものとして判定行に出る。** `hello` は "hello from ring 3" を
    // 送るので、`syscall-test` が送ったはずのバイト列と食い違う。
    CriticalTest {
        name: "spawn-keep-child-records",
        feature: "spawn-keep-child-records",
        expected_markers: &["user-run: syscall-test", "halting"],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S11-8: `write` が fd を見ずに、何番でも出力する。**出力はそのまま現れるので、
    // 雑に見ると正しく動いているように見える。** **見えないのは「開いていない番号が
    // 拒まれること」のほうである。**
    CriticalTest {
        name: "write-ignores-fd",
        feature: "write-ignores-fd",
        expected_markers: &[
            "user-run: syscall-test exited with status 45",
            "write(3, ...) did not return -EBADF",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S11-9: `write` が要求された長さの半分だけ書いて返す。**主張は「`write` は
    // 要求した長さを全部書く。書けなければ呼び出し側が繰り返す」である。**
    // **24 バイト以下は半分にしない**——asm で直に `write` を呼ぶ既存の 4 本は
    // 繰り返しを持たず、そこを半分にすると `ls` と `cat` が起こされる前に止まる。
    // **繰り返しの経路が実際に通ることも、この構成で確かめている**
    // （`ls` の 30 バイトの一覧が 2 周で出て、出力は欠けない）。
    CriticalTest {
        name: "write-half-only",
        feature: "write-half-only",
        expected_markers: &[
            "user-run: syscall-test exited with status 47",
            "did not return the number of bytes it was given",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S11-7: argv の量の問題を -E2BIG でなく -EINVAL で返す。**どちらも「引数が
    // 受け付けられない」を意味するので、雑に見ると同じに見える**（S10-b の 4 つと
    // 同じ族）。**Linux は分けている**——`execve` は長すぎる引数に `E2BIG` を返す。
    // **「値が変」と「量が多い」は、呼び出し側の直し方が違う。**
    CriticalTest {
        name: "spawn-e2big-as-einval",
        feature: "spawn-e2big-as-einval",
        expected_markers: &[
            "user-run: syscall-test exited with status 42",
            "an argv with more entries than the limit did not return -E2BIG",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S11-7: 写した argv の最後の 1 本を落とす。**終端の扱いを 1 つずらす形で、
    // 雑に見ると「ちゃんと切り分けている」ように見える。** 子が受け取る `argc` が
    // 1 つ少なくなり、`spawn-test` の検算が捕まえる。
    CriticalTest {
        name: "spawn-argv-drop-last",
        feature: "spawn-argv-drop-last",
        expected_markers: &[
            "user-run: syscall-test exited with status 40",
            "the grandchild was not refused",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S11-5: 入れ子の遠征から戻す RSP0 を、親ではなく子自身の遠征スタックの上端に
    // する。**入れ子でないうちはこの経路を通らないので、入れ子になった瞬間だけ
    // 壊れる。** すぐには壊れず、次に子を起こしたときに親のフレームを踏む——
    // **原因から遠いので、`spawn` が戻り先の RSP0 を突き合わせて捕まえる。**
    CriticalTest {
        name: "spawn-child-rsp0",
        feature: "spawn-child-rsp0",
        expected_markers: &[
            "spawn: RSP0 came back as",
            "the parent's next kernel entry would land on the wrong stack",
            "halting",
        ],
        forbidden_markers: &["user-load: syscall-test ran as a process"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S9-a: 容量超過の errno を分ける前へ戻す。**この分岐は S9-a で初めて通るように
    // なった経路である。** 通り始めたばかりの経路を手で1度確かめただけにしないため、
    // 永続の破壊として置く。次に dispatch を触ったときに落ちる。
    CriticalTest {
        name: "copy-skip-validate",
        feature: "syscall-test-copy-skip-validate",
        expected_markers: &[
            "syscall: checksum case 'kernel pointer' expected reject",
            "halting",
        ],
        forbidden_markers: &["syscall: checksum round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // copy_from_user が len を 1 バイト超えて読む（M5-f-2-2）。末尾の余分な既知バイトが総和へ
    // 混ざり、内容往復のチェックサムが決定的に食い違って halt する。
    CriticalTest {
        name: "copy-overrun",
        feature: "syscall-test-copy-overrun",
        expected_markers: &["syscall: checksum mismatch", "halting"],
        forbidden_markers: &["syscall: checksum round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// higher-half（B-2a-5）の破壊確認。「cpu_reset が起きた」だけを合格条件に
/// しない（どんな理由で死んでも合格になり検査にならない）。各テストが「期待した
/// 箇所で死んだ」ことを、シリアルの位置署名（到達した行 present / その先へ進んで
/// いない行 absent）と「定常状態に到達していない（heartbeat が 1 本も出ない）」の
/// AND で判定する。
///
/// **実測メモ（B-2a-5）**: これらはトリプルフォルト（cpu_reset）しない。
/// (a)(b) はトランポリン/最初期の #PF が我々の IDT 導入前に起きるため UEFI の IDT が
/// 拾い、ファームウェアが制御を取り戻す（-d int に firmware 領域の RIP が残る）。
/// (c) は M2-d 切り替え後に高位で #PF し、定常へ進めず停止する。いずれも cpu_reset は
/// 増えない。したがって判定は位置署名 + 定常未到達で行い、cpu_reset 数と firmware RIP は
/// 参考情報として出す（cpu_reset を必須条件にしない、が実測でより強く正当化された）。
struct HighhalfTest {
    name: &'static str,
    feature: &'static str,
    /// シリアルに現れるべき行（そこまで到達した証拠）。
    present_markers: &'static [&'static str],
    /// シリアルに現れてはいけない行（その先へ進んでいないことの証拠）。
    absent_markers: &'static [&'static str],
}

const HIGHHALF_TESTS: &[HighhalfTest] = &[
    // (a) 初期 PML4[0]（恒等）を not-present にする。mov cr3 直後の低位命令フェッチが
    // 解決できず #PF。我々の IDT 導入前なので UEFI の IDT が拾い、ファームウェアへ戻る。
    // bootloader はトランポリンへ到達しているが、カーネルの最初のログは出ない。
    // (a) と (b) は同一署名（トランポリンとカーネルの間で死ぬ）。
    HighhalfTest {
        name: "no-identity-in-boot-pt",
        feature: "highhalf-no-identity-in-boot-pt",
        present_markers: &["kernel entry: VMA"],
        absent_markers: &["ZaytOS kernel: entered _start"],
    },
    // (b) PDPT_high のエントリを 510→509 へずらす。高位 _start への jmp 先が未マップで
    // #PF。署名は (a) と同じ。
    HighhalfTest {
        name: "bad-high-slot",
        feature: "highhalf-bad-high-slot",
        present_markers: &["kernel entry: VMA"],
        absent_markers: &["ZaytOS kernel: entered _start"],
    },
    // (c) 本流テーブルからカーネル高位マッピングを外す。高位到達し、切替前の必須マッピング
    // 検証も通過（それは物理範囲を見るので高位マッピングの欠落を捕まえない。実測で確定）した
    // うえで、M2-d の CR3 切り替え後に高位で #PF して停止する。
    HighhalfTest {
        name: "no-kernel-high-in-live-table",
        feature: "highhalf-no-kernel-high-in-live-table",
        present_markers: &["higher-half: arrived at high VA"],
        absent_markers: &["paging: CR3 switch verified"],
    },
    // B-2b-4: 恒等除去の 5a 復帰。他の highhalf 破壊と違い「どこかで死ぬ」テストではなく、
    // 恒等除去点で remove_identity を呼び、必須領域のヒープ高位 VA を解決不能な高位 VA
    // （空の PML4[257] = 0xffff808000000000）へ差し替えて step4 を失敗させる。5a が PML4[0] を
    // 書き戻し（restored PML4[0]）、恒等が実際に復活し（revived=true）、除去を完了せず
    // （done は出ない）halt する。除去点は start_timer より前なので heartbeat=0 も成立する。
    HighhalfTest {
        name: "remove-verify-fail",
        feature: "highhalf-remove-verify-fail",
        present_markers: &[
            "required [heap high VA] 0xffff808000000000: FAILED",
            "verification FAILED. restored PML4[0]",
            "revived=true",
        ],
        absent_markers: &["identity-removal: done"],
    },
    // B-2b-4(e): ヒープ初期化を低位（heap_start=物理）へ戻す。恒等除去は begin→clear まで進み、
    // 5b のフラッシュ後に step6 が high_mapped（低位ヒープ上）を辿って #PF で死ぬ（removal は
    // 除去は既定どおり done まで完走し（high_mapped はスタックなので除去自身は生き残る）、その後
    // 1181 のヒープスモークテストが低位ヒープ（`heap_start` 物理）をデレフして #PF で死ぬ。**この
    // テストは #PF ダンプ（vector=14 + cr2 が低位＝物理ヒープ域）が出ることが成功署名**であり、
    // フレーク判断の一般手順（`exception: vector=` は回帰）とは区別される意図的破壊（除去より前に
    // ヒープを高位化する順序の必要性を実証。verification-coverage 参照）。cr2=0x00000000… で低位を確認。
    HighhalfTest {
        name: "remove-before-highify",
        feature: "highhalf-remove-before-highify",
        present_markers: &[
            "identity-removal: done",
            "exception: vector=14",
            "cr2=0x00000000",
        ],
        absent_markers: &[],
    },
    // B-2b-4(e): 恒等除去の直後に意図的 panic。除去は done まで完走し、その後パニックダンプが出る。
    // **パニックダンプが出ることが成功署名**（パニック経路がシリアル I/O・レジスタ値のみ・walk
    // なしで恒等非依存であり、恒等を外した後も動くことの実証）。同じく一般手順とは区別される。
    HighhalfTest {
        name: "panic-after-remove",
        feature: "highhalf-panic-after-remove",
        present_markers: &[
            "identity-removal: done",
            "intentional panic right after identity removal",
        ],
        absent_markers: &[],
    },
];

/// トランポリンのコード先頭 24 バイト（B-2a-2/B-2a-3b で 2 度、再リンク・
/// B-1 撤去を跨いで不変を実証した期待リテラル）。rel32 はすべて .text.trampoline
/// 内なので、他の変更で動かない。sabotage (d) はここへ 1 命令挿入して食い違わせる。
const EXPECTED_TRAMPOLINE_BYTES: [u8; 24] = [
    0x48, 0x8b, 0x05, 0x11, 0x00, 0x00, 0x00, // mov rax, [rip + zaytos_tramp_pml4]
    0x0f, 0x22, 0xd8, // mov cr3, rax
    0x48, 0x8b, 0x25, 0x0f, 0x00, 0x00, 0x00, // mov rsp, [rip + zaytos_tramp_stack]
    0xff, 0x25, 0x11, 0x00, 0x00, 0x00, // jmp [rip + zaytos_tramp_entry]
    0x90, // p2align 3 のパディング
];

/// ビルド済み kernel.elf の入口（トランポリン）先頭 24 バイトを読む。
///
/// 新規クレート依存を増やさず、既存の `common::elf` を再利用する。入口は
/// `.text.trampoline` の先頭に置いてあり、入口を含む PT_LOAD セグメントの
/// ファイル内容から `entry - p_vaddr` オフセットで取り出す。
fn trampoline_bytes(kernel_elf: &Path) -> Result<[u8; 24]> {
    let bytes =
        fs::read(kernel_elf).with_context(|| format!("failed to read {}", kernel_elf.display()))?;
    let elf = common::elf::Elf::parse(&bytes)
        .map_err(|e| anyhow::anyhow!("failed to parse {} as ELF: {e:?}", kernel_elf.display()))?;
    let entry = elf.entry_point;
    let seg = elf
        .load_segments()
        .find(|s| entry >= s.p_vaddr && entry < s.p_vaddr + s.p_memsz)
        .context("no PT_LOAD segment contains the entry point")?;
    let data = elf
        .segment_data(&seg)
        .map_err(|e| anyhow::anyhow!("the entry segment lies outside the file: {e:?}"))?;
    let offset = (entry - seg.p_vaddr) as usize;
    let slice = data
        .get(offset..offset + 24)
        .context("the entry point is too close to the end of its segment")?;
    let mut out = [0u8; 24];
    out.copy_from_slice(slice);
    Ok(out)
}

/// カーネルが起動したことを示す、シリアルログの既知の行。
///
/// kernel の `kernel_main` が最初に出す行（`common::log` の INFO 形式）。
/// これがログに無ければ、カーネルは走っていない。
const KERNEL_STARTED_MARKER: &str = "[INFO] ZaytOS kernel: entered _start";

/// カーネルが**起動しきった**ことを示す行。
///
/// M4-d-2 より前は `kernel: halting` が到達点の目印だった。タイマを入れて
/// カーネルが停止しなくなったため、その行はもう出ない。代わりに
/// 「最初のタイマ割り込みが届いた」を到達点とする。ここまで来ていれば、
/// GDT / IDT / ページング / ヒープ / コンソール / PIC / PIT のすべてが
/// 動いており、割り込みも配送されている。
///
/// [`KERNEL_STARTED_MARKER`] とは役割が違う。あちらは「そもそもカーネルが
/// 走ったか」（OVMF の起動失敗との切り分け）を見るためのもので、こちらは
/// 「最後まで通ったか」を見る。
const KERNEL_BOOT_COMPLETE_MARKER: &str = "timer: first tick arrived as vector 0x20";

/// bootloader が起動したことを示す、シリアルログの既知の行。
///
/// panic-test は bootloader を検証するもので、kernel へ到達する前に panic
/// する。したがって kernel の marker ではなくこちらで起動を判定する。
const BOOTLOADER_STARTED_MARKER: &str = "[INFO] ZaytOS bootloader: serial log established";

/// OVMF がまれにカーネルを起動せず、シェルやアイドルループへフォールバック
/// することがある（`docs/troubleshooting.md` 2026-07-19 の記録）。この場合
/// ファームウェアのタイマ割り込み（ベクタ 0x20）が延々と記録され、RIP は
/// ファームウェア領域（おおむね 0x0f00_0000 以上、実 RAM 256MiB の外）を指す。
const FIRMWARE_REGION_START: u64 = 0x0f00_0000;

/// テストの前提（カーネルが起動したか）を判定した結果。
enum BootOutcome {
    /// カーネルが起動した。テスト結果を信頼してよい。
    Started,
    /// カーネルが起動しなかった。テストの成否ではなく環境の問題。
    DidNotStart {
        /// ファームウェア領域を指す RIP を `-d int` ログから拾えた場合。
        firmware_rip: Option<u64>,
    },
}

/// シリアルログと QEMU デバッグログから、対象が起動したかを判定する。
///
/// `started_marker` は起動を示す既知の行（kernel なら
/// [`KERNEL_STARTED_MARKER`]、bootloader なら [`BOOTLOADER_STARTED_MARKER`]）。
///
/// **テストの FAIL を報告する前に必ず呼ぶこと。** 「実装の問題」と「OVMF の
/// 起動フレーキネス」を取り違えると、存在しないバグを追いかけることになる
/// （実際に M4-c-1 でこの取り違えが起きかけた）。
fn classify_boot(serial: &str, qemu_debug: &str, started_marker: &str) -> BootOutcome {
    if serial.contains(started_marker) {
        return BootOutcome::Started;
    }

    // 起動していない。裏付けとして、ファームウェア領域を指す RIP を探す。
    // `-d int` の RIP 行は "RIP=000000000f6e973c ..." の形。
    let firmware_rip = qemu_debug.lines().rev().find_map(|line| {
        let rest = line.trim_start().strip_prefix("RIP=")?;
        let hex = rest.split_whitespace().next()?;
        let value = u64::from_str_radix(hex, 16).ok()?;
        (value >= FIRMWARE_REGION_START).then_some(value)
    });

    BootOutcome::DidNotStart { firmware_rip }
}

/// 起動失敗を報告する。テスト FAIL とは別物として扱う。
///
/// 戻り値の `Err` は「テストが落ちた」ではなく「起動しなかったので判定
/// できない」ことを表す。呼び出し側はこのメッセージで両者を区別する。
///
/// # 原因を断定しない
///
/// 以前ここは「OVMF の起動フレーキネスである」と断定して書いていた。
/// **その文言が、まったく別の原因を 2 度覆い隠した。**
///
/// - varstore を使い回していたため OVMF の起動項目が蓄積していた
/// - QEMU の monitor ソケットのパスが `sun_path` の 108 バイト上限を
///   超えており、QEMU がソケットを作れずに終了していた
///
/// どちらも症状は「起動マーカーが出ない」で同一だが、原因も直し方も違う。
/// 便利なラベルを貼ると、そこで調査が止まる。断定するのは観測した事実
/// （マーカーが無い、RIP がどこを指していた）だけにして、原因は候補を
/// 並べるにとどめる。
fn report_did_not_start(
    context: &str,
    firmware_rip: Option<u64>,
    qemu_exit: Option<&str>,
) -> Result<()> {
    println!("{context}: TARGET DID NOT START (not a test failure)");
    println!("{context}:   observed: the serial log has no start-up marker line");
    match firmware_rip {
        Some(rip) => println!(
            "{context}:   observed: qemu -d int shows RIP={rip:#018x} in the firmware region \
             (>= {FIRMWARE_REGION_START:#x}), so qemu ran and the firmware kept executing"
        ),
        None => println!(
            "{context}:   observed: no firmware RIP in the qemu log either, so qemu may not \
             have reached the firmware at all"
        ),
    }
    if let Some(status) = qemu_exit {
        println!("{context}:   observed: qemu exited on its own ({status})");
    }
    println!("{context}:   this check does not identify the cause. Candidates:");
    println!("{context}:     - OVMF booted but fell back to its shell / idle loop");
    println!("{context}:     - qemu never started (bad arguments, missing OVMF, port in use)");
    println!(
        "{context}:     - the monitor socket path exceeded the {SOCKET_PATH_LIMIT}-byte \
         sun_path limit"
    );
    println!("{context}:   see docs/troubleshooting.md 2026-07-22 for two past instances");
    bail!("{context}: kernel did not start (environment, not the code); re-run")
}

/// UNIX ドメインソケットのパス長の上限（`sockaddr_un::sun_path`）。
///
/// 終端の NUL を含めて 108 バイト。超えると `bind` に失敗する。QEMU は
/// エラーを出して即座に終了するため、症状は「ゲストが 1 行も出力しない」
/// になり、起動失敗と見分けがつかない。実際にこれで 8 回連続の失敗を
/// 「OVMF の起動フレーキネス」と誤認しかけた。
const SOCKET_PATH_LIMIT: usize = 108;

/// monitor ソケットのパスが `sun_path` の上限に収まることを確かめる。
///
/// **起動する前に弾く。** 超えたまま起動すると、QEMU が黙って終了して
/// 「起動しなかった」としか分からない。ここで長さを指摘すれば、
/// 作業ディレクトリが深すぎることがその場で分かる。
fn ensure_socket_path_fits(socket: &Path) -> Result<()> {
    let length = socket.as_os_str().as_bytes().len();
    if length < SOCKET_PATH_LIMIT {
        return Ok(());
    }
    bail!(
        "the qemu monitor socket path is {length} bytes, which does not fit in the \
         {SOCKET_PATH_LIMIT}-byte sun_path limit: {}\n\
         move the workspace to a shorter path; qemu would exit without producing any \
         guest output, which is indistinguishable from a boot failure",
        socket.display()
    )
}

// パニックハンドラの出力（bootloader/src/panic.rs）と対応する、回帰チェック用の
// 目印文字列。フォーマットを変更した場合はここも合わせて更新すること。
const PANIC_MARKER_HEADER: &str = "[ERROR] panic:";
const PANIC_MARKER_HALT: &str = "halting (cli + hlt loop)";
const PANIC_TEST_TIMEOUT: Duration = Duration::from_secs(10);
const PANIC_TEST_POLL_INTERVAL: Duration = Duration::from_millis(100);

const DEFAULT_SCREENSHOT_WAIT: Duration = Duration::from_secs(8);
const MONITOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SCREENDUMP_FILE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    const USAGE: &str = "usage: cargo xtask check [--full | --commit]\n       cargo xtask flaky\n       cargo xtask run [--panic-test] [--gui] [--gtk] [--gfx-test] [--kvm] [--no-limit] [--manual] [--key-probe] [--keep-disk | --rebuild-disk]\n       cargo xtask run --exception-test <kind>\n       cargo xtask run --critical-test <kind>\n       cargo xtask run --interrupt-test <kind>\n       cargo xtask run --paging-test <kind>\n       cargo xtask run --stack-test <kind>\n       cargo xtask run --task-test <kind>\n       cargo xtask run --ring3-test <kind>\n       cargo xtask run --syscall-test <kind>\n       cargo xtask run --acpi-test <kind>\n       cargo xtask run --acpi-smp-test\n       cargo xtask run --apic-test <kind>\n       cargo xtask run --apic-decode-test\n       cargo xtask run --ioapic-test <kind>\n       cargo xtask run --lapic-timer-test <kind>\n       cargo xtask run --drift-test [MINUTES] [--smp N]
       cargo xtask run --shell-test [--drop-arrows | --drop-esc]\n       cargo xtask run --ansi-test [--sabotage FEATURE]\n       cargo xtask run --zi-test [--sabotage FEATURE]\n       cargo xtask run --view-test [--sabotage FEATURE]
       cargo xtask run --fs-extract [--sabotage FEATURE]\n       cargo xtask run --pci-test [--sabotage FEATURE]\n       cargo xtask run --virtio-test [--sabotage FEATURE]\n       cargo xtask run --virtio-irq-test [--sabotage FEATURE]
       cargo xtask run --persist-test [--rebuild-between]
       cargo xtask run --persist-zi-test [--rebuild-between]
       cargo xtask run --persist-env-test [--rebuild-between]
       cargo xtask run --keymap-test [--sabotage]
       cargo xtask run --fp-test [--sabotage FEATURE]
       cargo xtask check [--update-reference]   (ホストテストの名前の集合を取り直す)
       cargo xtask run --boot-log-diff [--update-reference]
       cargo xtask run --calibration-spread [N]\n       cargo xtask run --highhalf-test <kind>\n       cargo xtask screenshot [output.png] [--wait-secs N] [--gfx-test] [--kvm]\n       cargo xtask gen-font";

    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => {
            let rest = &args[1..];
            let panic_test = rest.iter().any(|a| a == "--panic-test");
            let gui = rest.iter().any(|a| a == "--gui");
            let gfx_test = rest.iter().any(|a| a == "--gfx-test");
            let kvm = rest.iter().any(|a| a == "--kvm");
            let no_limit = rest.iter().any(|a| a == "--no-limit");
            // **手で触るための起動（zi-e 前の手当て）。** 上限を外し、
            // 記録を `cpu_reset` だけに絞る（[`cmd_run`] の doc）。
            let manual = rest.iter().any(|a| a == "--manual");
            // **打鍵の切り分け（zi-e の後の手当て）。** シェルを起こさず、
            // 取り出したスキャンコードを生のままシリアルへ出す構成で建てる
            // （[`build_kernel_for_key_probe`]）。
            let key_probe = rest.iter().any(|a| a == "--key-probe");
            // **窓を GTK で開く（zi-e）。** **窓の既定は SDL である**
            // ——GTK は JIS 固有キーを落とす（実測。[`DisplayMode::Sdl`]）。
            // **これは退路で、SDL で窓が開かない環境のために残してある。**
            let gtk = rest.iter().any(|a| a == "--gtk");
            if let Some(index) = rest.iter().position(|a| a == "--lapic-timer-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = LAPIC_TIMER_TESTS.iter().map(|t| t.name).collect();
                    format!("--lapic-timer-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_lapic_timer_test(kind);
            }
            // **AP のティックのレートをホストの実時間と突き合わせる（S4-a）。**
            // 表を持たない単独の検査なので、種別を取らない。
            if rest.iter().any(|a| a == "--ap-timer-rate") {
                return cmd_ap_timer_rate();
            }
            if let Some(index) = rest.iter().position(|a| a == "--bkl-test") {
                let kind = rest
                    .get(index + 1)
                    .map(String::as_str)
                    .unwrap_or(BKL_TESTS[0].name);
                if let Some(test) = BKL_TIMEOUT_TESTS.iter().find(|t| t.name == kind) {
                    return cmd_marker_test(BKL_TIMEOUT_TESTS, "bkl-test", test.name, Some(2));
                }
                return cmd_marker_test(BKL_TESTS, "bkl-test", kind, None);
            }
            if rest.iter().any(|a| a == "--bkl-exclusion-proof") {
                return cmd_bkl_exclusion_proof();
            }
            if rest.iter().any(|a| a == "--kernel-entry-concurrency") {
                return cmd_kernel_entry_concurrency();
            }
            if let Some(index) = rest.iter().position(|a| a == "--ioapic-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = IOAPIC_SABOTAGE_TESTS.iter().map(|t| t.name).collect();
                    format!("--ioapic-test requires a kind ({})", names.join(" | "))
                })?;
                let test = IOAPIC_SABOTAGE_TESTS
                    .iter()
                    .find(|t| t.name == kind)
                    .with_context(|| format!("unknown --ioapic-test kind: {kind}"))?;
                return cmd_ioapic_sabotage(test.name, test.feature, test.expected);
            }
            if let Some(index) = rest.iter().position(|a| a == "--interrupt-test") {
                if rest.get(index + 1).map(String::as_str) == Some("keyboard") {
                    return cmd_keyboard_test();
                }
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = INTERRUPT_TESTS.iter().map(|t| t.name).collect();
                    format!("--interrupt-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(INTERRUPT_TESTS, "interrupt-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--paging-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = PAGING_TESTS.iter().map(|t| t.name).collect();
                    format!("--paging-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(PAGING_TESTS, "paging-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--stack-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = STACK_TESTS.iter().map(|t| t.name).collect();
                    format!("--stack-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(STACK_TESTS, "stack-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--task-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = TASK_TESTS.iter().map(|t| t.name).collect();
                    format!("--task-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(TASK_TESTS, "task-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--ring3-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = RING3_TESTS.iter().map(|t| t.name).collect();
                    format!("--ring3-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(RING3_TESTS, "ring3-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--syscall-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = SYSCALL_TESTS.iter().map(|t| t.name).collect();
                    format!("--syscall-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(SYSCALL_TESTS, "syscall-test", kind, None);
            }
            if rest.iter().any(|a| a == "--acpi-smp-test") {
                return cmd_marker_test(
                    ACPI_SMP_TESTS,
                    "acpi-smp-test",
                    ACPI_SMP_TESTS[0].name,
                    Some(2),
                );
            }
            if rest.iter().any(|a| a == "--fs-extract") {
                let features: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                return cmd_fs_image_extract(&features);
            }
            if rest.iter().any(|a| a == "--view-test") {
                let features: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                return cmd_view_test(&features);
            }
            if rest.iter().any(|a| a == "--zi-test") {
                let features: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                return cmd_zi_test(&features);
            }
            if rest.iter().any(|a| a == "--ansi-test") {
                let features: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                return cmd_ansi_test(&features);
            }
            if rest.iter().any(|a| a == "--pci-test") {
                let features: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                return cmd_pci_test(&features);
            }
            if rest.iter().any(|a| a == "--virtio-irq-test") {
                let features: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                return cmd_virtio_irq_test(&features);
            }
            if rest.iter().any(|a| a == "--virtio-test") {
                let features: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                return cmd_virtio_test(&features);
            }
            if rest.iter().any(|a| a == "--shell-test") {
                // **破壊を 1 つだけ回す口（f-2）。**
                //
                // **以前は `--full` の中からしか回せなかった。** **`CLAUDE.md` の
                // 「判定を直したら、その判定が捕まえるはずの破壊をその場で走らせる」を
                // 満たすには、1 つだけ回せる必要がある**——`--virtio-test` と
                // 同じ `--sabotage` の形にした。
                let sabotage = rest
                    .iter()
                    .enumerate()
                    .find(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .and_then(|(i, _)| rest.get(i + 1))
                    .map(|name| name.as_str());
                let mode = match sabotage {
                    Some(name) => {
                        // **一覧に無い名前は断る。** **打ち間違いが「既定の回」に
                        // 化けると、破壊が捕まったことにならない。**
                        let Some(known) = SHELL_TEST_SABOTAGES
                            .iter()
                            .find(|entry| **entry == name)
                            .copied()
                        else {
                            bail!(
                                "xtask run --shell-test --sabotage: {name:?} is not one of the \
                                 shell sabotages ({SHELL_TEST_SABOTAGES:?})"
                            );
                        };
                        ShellTestMode::MustFail(known)
                    }
                    None if rest.iter().any(|a| a == "--drop-arrows") => {
                        ShellTestMode::ArrowsDropped
                    }
                    None if rest.iter().any(|a| a == "--drop-esc") => ShellTestMode::EscDropped,
                    None => ShellTestMode::Normal,
                };
                return cmd_shell_test(mode);
            }
            // **持ち越しの判定（P-a）。**
            if rest.iter().any(|a| a == "--utf8-test") {
                let sabotage: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                let expect_pass = sabotage.is_empty();
                return cmd_utf8_test(&sabotage, expect_pass);
            }
            // **FP の状態の判定（B-a。`ADR-0058`）。**
            if rest.iter().any(|a| a == "--fp-test") {
                let sabotage: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                let expect_pass = sabotage.is_empty();
                return cmd_fp_test(&sabotage, expect_pass);
            }
            // **Tab の補完の判定（TAB-1）。**
            if rest.iter().any(|a| a == "--complete-test") {
                let sabotage: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                let expect_pass = sabotage.is_empty();
                return cmd_complete_test(&sabotage, expect_pass);
            }
            // **履歴の持ち越しの判定（HI-1）。**
            if rest.iter().any(|a| a == "--history-test") {
                let sabotage: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                let expect_pass = sabotage.is_empty();
                return cmd_history_test(&sabotage, expect_pass);
            }
            // **起動時の設定の判定（PR-1）。**
            if rest.iter().any(|a| a == "--profile-test") {
                let sabotage: Vec<&str> = rest
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| *a == "--sabotage" && rest.get(i + 1).is_some())
                    .filter_map(|(i, _)| rest.get(i + 1).map(|s| s.as_str()))
                    .collect();
                let expect_pass = sabotage.is_empty();
                return cmd_profile_test(&sabotage, expect_pass);
            }
            if rest.iter().any(|a| a == "--keymap-test") {
                return cmd_keymap_test(rest.iter().any(|a| a == "--sabotage"));
            }
            if rest.iter().any(|a| a == "--persist-env-test") {
                return cmd_persist_env_test(
                    rest.iter().any(|a| a == "--rebuild-between"),
                    rest.iter().any(|a| a == "--ignore-file"),
                );
            }
            if rest.iter().any(|a| a == "--persist-zi-test") {
                return cmd_persist_zi_test(rest.iter().any(|a| a == "--rebuild-between"));
            }
            if rest.iter().any(|a| a == "--persist-test") {
                return cmd_persist_test(rest.iter().any(|a| a == "--rebuild-between"));
            }
            if rest.iter().any(|a| a == "--boot-log-diff") {
                let update = rest.iter().any(|a| a == "--update-reference");
                return cmd_boot_log_diff(update);
            }
            if let Some(index) = rest.iter().position(|a| a == "--drift-test") {
                let minutes = rest
                    .get(index + 1)
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(DRIFT_TEST_DEFAULT_MINUTES);
                let smp = rest
                    .iter()
                    .position(|a| a == "--smp")
                    .and_then(|i| rest.get(i + 1))
                    .and_then(|v| v.parse::<u32>().ok());
                return cmd_drift_test(minutes, smp);
            }
            if rest.iter().any(|a| a == "--acpi-smp4-test") {
                return cmd_marker_test(
                    ACPI_SMP4_TESTS,
                    "acpi-smp-test",
                    ACPI_SMP4_TESTS[0].name,
                    Some(4),
                );
            }
            if let Some(index) = rest.iter().position(|a| a == "--smp-ap-test") {
                let kind = rest
                    .get(index + 1)
                    .map(String::as_str)
                    .unwrap_or(SMP_AP_TESTS[0].name);
                return cmd_marker_test(SMP_AP_TESTS, "smp-ap-test", kind, Some(2));
            }
            if rest.iter().any(|a| a == "--smp-tramp-test") {
                return cmd_marker_test(
                    SMP_TRAMP_TESTS,
                    "smp-tramp-test",
                    SMP_TRAMP_TESTS[0].name,
                    Some(2),
                );
            }
            if rest.iter().any(|a| a == "--percpu-test") {
                return cmd_marker_test(PERCPU_TESTS, "percpu-test", PERCPU_TESTS[0].name, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--acpi-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = ACPI_TESTS.iter().map(|t| t.name).collect();
                    format!("--acpi-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(ACPI_TESTS, "acpi-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--apic-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = APIC_TESTS.iter().map(|t| t.name).collect();
                    format!("--apic-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(APIC_TESTS, "apic-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--calibration-spread") {
                let runs = rest
                    .get(index + 1)
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(DEFAULT_CALIBRATION_RUNS);
                return cmd_calibration_spread(runs);
            }
            if rest.iter().any(|a| a == "--apic-decode-test") {
                return cmd_marker_test(
                    APIC_DECODE_TESTS,
                    "apic-decode-test",
                    APIC_DECODE_TESTS[0].name,
                    None,
                );
            }
            if let Some(index) = rest.iter().position(|a| a == "--highhalf-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = HIGHHALF_TESTS.iter().map(|t| t.name).collect();
                    format!(
                        "--highhalf-test requires a kind ({} | trampoline-absolute-ref)",
                        names.join(" | ")
                    )
                })?;
                // (d) はビルド + 静的バイト検査（QEMU 不要）。(a)(b)(c) は QEMU で判定。
                if kind == "trampoline-absolute-ref" {
                    return cmd_highhalf_trampoline_check(
                        &workspace_root()?,
                        &["highhalf-trampoline-absolute-ref"],
                        false,
                    );
                }
                return cmd_highhalf_test(kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--critical-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = CRITICAL_TESTS.iter().map(|t| t.name).collect();
                    format!("--critical-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(CRITICAL_TESTS, "critical-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--exception-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = EXCEPTION_TESTS.iter().map(|t| t.name).collect();
                    format!("--exception-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_exception_test(kind);
            }
            cmd_run(&RunOptions {
                panic_test,
                keep_disk: rest.iter().any(|a| a == "--keep-disk"),
                rebuild_disk: rest.iter().any(|a| a == "--rebuild-disk"),
                gui,
                gtk,
                gfx_test,
                kvm,
                // **`--manual` は上限を外す**（`cmd_run` の doc）。
                no_limit: no_limit || manual,
                manual,
                key_probe,
            })
        }
        Some("check") => {
            let full = args[1..].iter().any(|a| a == "--full");
            let commit = args[1..].iter().any(|a| a == "--commit");
            if full && commit {
                bail!("--full already includes everything --commit runs; pass one of them");
            }
            cmd_check(
                full,
                commit,
                args[1..].iter().any(|a| a == "--update-reference"),
            )
        }
        // `--full` から外した確率的な項目を手で回す。外した項目を回す手段が
        // なければ、外すことは「守らないと決める」ことになる。
        Some("flaky") => cmd_flaky(),
        Some("screenshot") => cmd_screenshot(&args[1..]),
        Some("gen-font") => font::generate(&workspace_root()?),
        Some(other) => bail!("unknown xtask subcommand: {other}\n\n{USAGE}"),
        None => bail!("missing xtask subcommand\n\n{USAGE}"),
    }
}

/// QEMU の `-serial` に渡す送り先。通常運用は人間がその場で読める `stdio`、
/// panic-test 回帰チェック・screenshot はプログラムから内容を検査できる
/// `file` を使う。
enum SerialSink {
    Stdio,
    File(PathBuf),
}

/// QEMU の `-display` バックエンド。既定は `none`（ADR-0003: シリアルログを
/// 唯一の観測手段とする）。`--gui` 指定時のみ実際のウィンドウを開く。
enum DisplayMode {
    None,
    /// 窓を開ける。**窓を開けるときの既定である**（zi-e）。
    ///
    /// # なぜ GTK ではなく SDL が既定なのか
    ///
    /// **GTK は JIS 固有キーを落とす（実測）。** 運用者の JIS キーボードで、
    /// **`ろ`（`0x73`）と `¥`（`0x7D`）だけが届かなかった**——他のキーは
    /// 押下も離鍵も届く。**SDL では両方とも届く。**
    ///
    /// **打つ人が居るのは窓を開けるときだけである。** **打てないキーが
    /// ある側を既定に残す理由が無い。**
    ///
    /// 経緯は `docs/troubleshooting.md` にある。
    Sdl,
    /// GTK で開く（`--gtk`）。**退路である。**
    ///
    /// **SDL で窓が開かない環境があり得る**（別の機械、別の版）。
    /// **既定を替えるときは、替える前のものを選べる形で残す。**
    Gtk,
}

/// QEMU のアクセラレータ。既定は TCG（純粋エミュレーション）。
///
/// 既定を TCG のままにしているのは、これまでの全マイルストーンを TCG で
/// 検証してきており、既定を変えると挙動差の切り分け軸が増えるため。
/// KVM は計測時にだけ `--kvm` で明示的に選ぶ（ADR-0015 Addendum）。
enum Accelerator {
    Tcg,
    Kvm,
}

/// `qemu_launch_args` に渡す設定一式。引数が増えてきたため、位置引数の
/// 取り違えを避けるためにまとめている。
struct QemuLaunchOptions<'a> {
    ovmf_code: &'a Path,
    ovmf_vars: &'a Path,
    esp_dir: &'a Path,
    serial: &'a SerialSink,
    debug_log: &'a Path,
    display: DisplayMode,
    /// `Some` の場合、この UNIX ソケットパスで QEMU monitor (HMP) を
    /// server モードで待ち受けさせる（screenshot サブコマンド用）。
    monitor_socket: Option<&'a Path>,
    accelerator: Accelerator,
    /// `-d` に何を渡すか（zi-e 前の手当て）。**既定は `int,cpu_reset` である**
    /// （`docs/coding-standards.md` の「QEMU の沈黙の失敗を必ず可視化する」）。
    debug_events: DebugEvents,
}

/// QEMU の `-d` に渡す種類（zi-e 前の手当て）。
///
/// # なぜ選べるようにしたか
///
/// **`int` は割り込み1件ごとに CPU の全状態を出す**（実測で 1 件あたり
/// 約 1.3KiB）。**シェルは `read(0)` を回して待つ**ので、**`int 0x80` が
/// 毎秒約 39,000 件出る**——**実測で、120 秒の起動で 5.6GiB になった。**
///
/// **自動検査では外さない。** あれは落ちた原因を追う唯一の記録である。
/// **手で触る経路（`--manual`）でだけ `cpu_reset` に絞る**——
/// **トリプルフォルトの再起動要因は残り、割り込みの列だけが消える。**
#[derive(Clone, Copy, PartialEq, Eq)]
enum DebugEvents {
    /// `int,cpu_reset`。自動検査の既定。
    IntAndCpuReset,
    /// `cpu_reset` だけ。手で触るときの選択。
    CpuResetOnly,
}

impl DebugEvents {
    fn as_qemu_value(self) -> &'static str {
        match self {
            DebugEvents::IntAndCpuReset => "int,cpu_reset",
            DebugEvents::CpuResetOnly => "cpu_reset",
        }
    }
}

/// `cargo xtask run`。
///
/// # `--manual`——運用者が手で触るための起動（zi-e 前の手当て）
///
/// **`--gui --manual` が、手で触るときの道である。** 2 つのことをする。
///
/// - **時間の上限を外す**（`--no-limit` と同じ）。**120 秒では手で触れない。**
/// - **`-d` を `cpu_reset` だけに絞る**（[`DebugEvents`]）。**`int` を付けた
///   ままでは記録が毎秒 48MiB 増える**（実測。シェルが `read(0)` を回して
///   待つので、`int 0x80` が毎秒約 39,000 件出る）。**上限を外したうえで
///   `int` を残すと、1 分あたり約 2.9GiB でディスクが埋まる。**
///
/// **自動検査の側の時間制限と記録は変えていない。** あちらは fail-fast の
/// 機構で、落ちた原因を追う唯一の記録である（`docs/coding-standards.md` の
/// 「QEMU の沈黙の失敗を必ず可視化する」）。
///
/// **失うもの**——**手で触っている間に例外が起きても、割り込みの列は残らない。**
/// **残るのは `cpu_reset`**（トリプルフォルトの再起動要因）**とシリアルである。**
/// **原因を追う段になったら `--manual` を外して起こし直すこと。**
/// `cargo xtask run` の指定。
///
/// **1 つずつ渡す形だと引数が 8 本になり、clippy が落ちる**
/// （`too_many_arguments`）。**旗を足すたびに呼び出し側 3 か所を直す形でも
/// あったので、まとめてある。**
struct RunOptions {
    panic_test: bool,
    /// `disk0.img` を作り直さずに起こす（P-c-3）。
    ///
    /// **`--manual` のときは既定でこちらである**（[`disk_for_run`]）。
    keep_disk: bool,
    /// `disk0.img` を作り直して起こす（P-c-3）。**`--manual` の既定を覆す。**
    rebuild_disk: bool,
    gui: bool,
    gtk: bool,
    gfx_test: bool,
    kvm: bool,
    no_limit: bool,
    manual: bool,
    key_probe: bool,
}

/// 手で起こすときに `disk0.img` を作り直すか（P-c-3。運用者の指示）。
///
/// # 境界は「打つ人が居るか」で引く
///
/// **`--manual` のときは持ち越しを既定にする。** **人が触るときは、
/// さっき保存したものが次の起動に在るのが自然である**——**いちいち旗を
/// 付けるほうが不自然である。**
///
/// **検査は毎回同じ像から始めたい**ので、**作り直しが既定であるべきである。**
/// **持ち越しの経路は `--full` の中で 2 項目しか通らない**
/// （[`cmd_persist_test`] と [`cmd_persist_zi_test`]）。
///
/// **この引き方には前例が 2 つある。** **窓を開けるときは SDL を使う**
/// （打つ人が居るのはそのときだけ）。**`--manual` のときだけ `-d int` を
/// 落とす。** **どれも「打つ人が居るか」で分けている。**
///
/// # 明示の旗は両方残す
///
/// **`--keep-disk` は `--manual` でないときに持ち越したい場合に要る。**
/// **`--rebuild-disk` は `--manual` の既定を覆す**——**「付けなければ消える」
/// を目で見る道がここに残る。**
///
/// **両方付いたら作り直しが勝つ。** **消えるほうが安全側だからである**
/// ——**作り直しは前の起動の中身を捨てるだけだが、持ち越しは
/// 「作り直したつもりの検査」を汚れた像の上で走らせる。**
fn disk_for_run(manual: bool, keep_disk: bool, rebuild_disk: bool) -> DiskImage {
    if rebuild_disk {
        return DiskImage::Rebuild;
    }
    if keep_disk || manual {
        DiskImage::Keep
    } else {
        DiskImage::Rebuild
    }
}

fn cmd_run(opts: &RunOptions) -> Result<()> {
    // **窓と上限の旗は [`run_interactive`] が読む。** ここが使うのは 3 つだけである。
    let RunOptions {
        panic_test,
        gfx_test,
        key_probe,
        ..
    } = *opts;
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, panic_test)?;
    // **打鍵の切り分けだけ、別の構成で建てる**（[`build_kernel_for_key_probe`]）。
    let kernel_elf = if key_probe {
        build_kernel_for_key_probe(&workspace_root, gfx_test)?
    } else {
        build_kernel(&workspace_root, gfx_test)?
    };
    let esp_dir = match disk_for_run(opts.manual, opts.keep_disk, opts.rebuild_disk) {
        DiskImage::Keep => {
            stage_esp_keeping_the_disk(&workspace_root, &bootloader_efi, &kernel_elf)?
        }
        DiskImage::Rebuild => stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?,
    };

    if panic_test {
        run_panic_test(&workspace_root, &ovmf_vars, &esp_dir)
    } else {
        run_interactive(&workspace_root, &ovmf_vars, &esp_dir, opts)
    }
}

/// `cargo xtask run` の既定の実行時間上限。
///
/// **M4-d-2 でカーネルが停止しなくなった。** タイマ割り込みで回り続けるため、
/// 放置すると `-d int` のデバッグログが増え続ける。実測で約 137KB/秒
/// （100Hz、1 ティックあたり 20 行強）なので、1 時間放置すれば 500MB に
/// 達する。「知らずに放置してディスクが埋まる」経路を塞ぐため、既定で
/// 打ち切る。`--no-limit` で解除できる。
const RUN_TIME_LIMIT: Duration = Duration::from_secs(120);

/// `-d int,cpu_reset` のログが増える速度（実測。TCG。KiB/秒）。
///
/// # 測り直した（zi-e 前の手当て）
///
/// **以前は 137 だった。** あれは M4-d-2 の実測で、**まだシェルが居らず、
/// 出ていたのはタイマ割り込みだけだった**（100Hz、1 ティックあたり 20 行強）。
///
/// **いまは約 49,000 である**（**370 倍**）。**駆動しているのはシェルである**
/// ——`read(0)` を回して待つので、**`int 0x80` が毎秒約 39,000 件出て、
/// 1 件ごとに CPU の全状態（約 1.3KiB）が記録される。**
///
/// **実測の内訳**（既定構成、120 秒、`-display none`）——
/// 記録は 6,021,246,698 バイト（5.6GiB）、事象は 4,657,255 件で、
/// **`v=80`（`int 0x80`）が 4,645,430 件（99.7%）**、`v=fe`（LAPIC タイマ）が
/// 11,423 件、`v=20` が 369 件、例外（`v=0d` / `v=0e` / `v=06`）が 19 件である。
///
/// **この値は「待つ形」を入れたら変わる。** シェルが眠るようになれば
/// `int 0x80` は消える。**測り直すこと。**
const DEBUG_LOG_GROWTH_KB_PER_SEC: u64 = 49_000;

fn run_interactive(
    workspace_root: &Path,
    ovmf_vars: &Path,
    esp_dir: &Path,
    opts: &RunOptions,
) -> Result<()> {
    let RunOptions {
        gui,
        gtk,
        kvm,
        no_limit,
        manual,
        ..
    } = *opts;
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        serial: &SerialSink::Stdio,
        debug_log: &debug_log,
        // **`--gtk` も窓を開ける**（`--gui` と一緒に書かなくてよい）。
        display: match (gui || gtk, gtk) {
            (true, false) => DisplayMode::Sdl,
            (true, true) => DisplayMode::Gtk,
            (false, _) => DisplayMode::None,
        },
        monitor_socket: None,
        accelerator: if kvm {
            Accelerator::Kvm
        } else {
            Accelerator::Tcg
        },
        debug_events: if manual {
            DebugEvents::CpuResetOnly
        } else {
            DebugEvents::IntAndCpuReset
        },
    });
    let debug_events = if manual {
        DebugEvents::CpuResetOnly
    } else {
        DebugEvents::IntAndCpuReset
    };
    println!(
        "qemu debug log (-d {}): {}",
        debug_events.as_qemu_value(),
        debug_log.display()
    );
    // M4-d-2 以降、カーネルは halt せずタイマで回り続ける。ログが増え続ける
    // ことを知らせておく。
    match debug_events {
        DebugEvents::IntAndCpuReset => println!(
            "note: the shell polls read(0), so every int 0x80 is logged with a full CPU dump. \
             The debug log grows at roughly {} MiB/s (measured). Pass --manual to drop `int`.",
            DEBUG_LOG_GROWTH_KB_PER_SEC / 1024
        ),
        DebugEvents::CpuResetOnly => println!(
            "note: --manual given; `-d` records cpu_reset only. A triple fault still leaves its \
             reset reason, but the interrupt trace is gone. Drop --manual to get it back."
        ),
    }
    if no_limit {
        println!("note: --no-limit given; qemu will run until you stop it (Ctrl-C).");
    } else {
        println!(
            "note: qemu will be stopped automatically after {} s (pass --no-limit to disable).",
            RUN_TIME_LIMIT.as_secs()
        );
    }
    if kvm {
        println!(
            "accelerator: KVM (measurement mode). exception logging via -d int is \
             largely unavailable; use the default TCG for debugging."
        );
    }

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context(
            "failed to launch qemu-system-x86_64 (is it installed? `apt install qemu-system-x86`)",
        )?;

    if no_limit {
        let status = child
            .wait()
            .context("failed to wait for qemu-system-x86_64")?;
        if !status.success() {
            bail!("qemu-system-x86_64 exited with {status}");
        }
        return Ok(());
    }

    // 上限まで待つ。途中で自分から終わった（パニック等）なら、そこで抜ける。
    let deadline = Instant::now() + RUN_TIME_LIMIT;
    loop {
        match child
            .try_wait()
            .context("failed to poll qemu-system-x86_64")?
        {
            Some(status) => {
                if !status.success() {
                    bail!("qemu-system-x86_64 exited with {status}");
                }
                return Ok(());
            }
            None => {
                if Instant::now() >= deadline {
                    // **概算ではなく実測を出す（zi-e 前の手当て）。**
                    // **以前は定数から掛け算していたので、実物と 370 倍
                    // 食い違っていても気づけなかった**（実際そうなっていた。
                    // 「16MB」と出しながら 5.6GiB 書いていた）。
                    let grown = fs::metadata(&debug_log).map(|m| m.len()).unwrap_or(0);
                    println!(
                        "\nreached the {} s limit; stopping qemu. The debug log is {} byte(s) \
                         ({} MiB).",
                        RUN_TIME_LIMIT.as_secs(),
                        grown,
                        grown / (1024 * 1024)
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(());
                }
                thread::sleep(PANIC_TEST_POLL_INTERVAL);
            }
        }
    }
}

/// パニックハンドラの回帰チェック。`panic-test` フィーチャ付きでビルドした
/// bootloader（起動完了直後に意図的に `panic!` する）を起動し、シリアル出力に
/// 期待どおりのパニックダンプが現れるかをポーリングで確認する。
///
/// bootloader はパニック後も `hlt` ループで動き続け自然終了しないため、目印を
/// 検出し次第（またはタイムアウトで）QEMU プロセスを強制終了する。
fn run_panic_test(workspace_root: &Path, ovmf_vars: &Path, esp_dir: &Path) -> Result<()> {
    let serial_log_path = workspace_root.join("target").join("panic-test-serial.log");
    let _ = fs::remove_file(&serial_log_path);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        serial: &SerialSink::File(serial_log_path.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        // パニック経路の回帰チェックは例外まわりの挙動を見るものなので、
        // 常に TCG で行う。
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the panic-test regression check")?;

    let deadline = Instant::now() + PANIC_TEST_TIMEOUT;
    let found = loop {
        if panic_markers_present(&serial_log_path) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    };

    let _ = child.kill();
    let _ = child.wait();

    let captured = read_lossy(&serial_log_path);
    let qemu = read_lossy(&debug_log);
    println!("--- panic-test: captured serial output ---\n{captured}--- end ---");

    if found {
        println!("panic-test: PASS (panic handler produced the expected dump and halted)");
        return Ok(());
    }

    // 失敗した。実装の問題か、そもそも bootloader が起動しなかったかを分ける。
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&captured, &qemu, BOOTLOADER_STARTED_MARKER)
    {
        return report_did_not_start("panic-test", firmware_rip, None);
    }

    bail!(
        "panic-test: FAIL (bootloader started but did not produce the expected \
         panic-handler output within {PANIC_TEST_TIMEOUT:?})"
    )
}

fn panic_markers_present(serial_log_path: &Path) -> bool {
    let contents = read_lossy(serial_log_path);
    contents.contains(PANIC_MARKER_HEADER) && contents.contains(PANIC_MARKER_HALT)
}

/// `cargo xtask screenshot [output.png] [--wait-secs N]` の引数を解釈する。
fn cmd_screenshot(args: &[String]) -> Result<()> {
    let mut wait = DEFAULT_SCREENSHOT_WAIT;
    let mut output_path = None;
    let mut gfx_test = false;
    let mut kvm = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gfx-test" => gfx_test = true,
            "--kvm" => kvm = true,
            "--wait-secs" => {
                i += 1;
                let value = args
                    .get(i)
                    .context("--wait-secs requires a value (seconds)")?;
                let secs: u64 = value
                    .parse()
                    .with_context(|| format!("invalid --wait-secs value: {value}"))?;
                wait = Duration::from_secs(secs);
            }
            other => output_path = Some(PathBuf::from(other)),
        }
        i += 1;
    }

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel(&workspace_root, gfx_test)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let output_path =
        output_path.unwrap_or_else(|| workspace_root.join("target").join("screenshot.png"));

    take_screenshot(
        &workspace_root,
        &ovmf_vars,
        &esp_dir,
        wait,
        &output_path,
        kvm,
    )
}

/// QEMU を monitor (HMP) 付きで起動し、`wait` だけ待ってから `screendump` を
/// 発行し、結果の PPM を PNG へ変換して `output_path` に保存する。
///
/// 既存のシリアル出力（`SerialSink`）・デバッグログ（`-D`）の分離は崩さない:
/// このコマンド専用のシリアルログファイルへ出力させ、ターミナルには
/// このコマンド自身の進捗メッセージのみを出す。
fn take_screenshot(
    workspace_root: &Path,
    ovmf_vars: &Path,
    esp_dir: &Path,
    wait: Duration,
    output_path: &Path,
    kvm: bool,
) -> Result<()> {
    // AF_UNIX のパス長制限 (108 バイト程度) を避けるため、ワークスペース内の
    // 長いパスではなく /tmp 配下の短い一意なパスを使う。
    let monitor_socket =
        PathBuf::from(format!("/tmp/zaytos-xtask-mon-{}.sock", std::process::id()));
    let _ = fs::remove_file(&monitor_socket);
    ensure_socket_path_fits(&monitor_socket)?;
    let serial_log = workspace_root.join("target").join("screenshot-serial.log");
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let ppm_path = workspace_root.join("target").join("screenshot.ppm");
    let _ = fs::remove_file(&ppm_path);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        serial: &SerialSink::File(serial_log),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: Some(&monitor_socket),
        accelerator: if kvm {
            Accelerator::Kvm
        } else {
            Accelerator::Tcg
        },
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for screenshot")?;

    println!("waiting {wait:?} for the boot sequence to settle before capturing...");
    thread::sleep(wait);

    let result = capture_screendump(&monitor_socket, &ppm_path);

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&monitor_socket);

    result?;

    let img = image::open(&ppm_path)
        .with_context(|| format!("failed to decode {}", ppm_path.display()))?;
    img.save(output_path)
        .with_context(|| format!("failed to save {}", output_path.display()))?;

    println!(
        "screenshot saved: {} ({}x{})",
        output_path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}

/// QEMU monitor (HMP) へ接続し `screendump <ppm_path>` を発行、ファイルが
/// 現れるまで待つ。
fn capture_screendump(monitor_socket: &Path, ppm_path: &Path) -> Result<()> {
    let mut stream = connect_monitor_with_retry(monitor_socket)?;
    writeln!(stream, "screendump {}", ppm_path.display())
        .context("failed to send screendump command to the QEMU monitor")?;
    stream.flush().ok();
    wait_for_file(ppm_path, SCREENDUMP_FILE_TIMEOUT)
}

fn connect_monitor_with_retry(socket_path: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + MONITOR_CONNECT_TIMEOUT;
    loop {
        match UnixStream::connect(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to connect to QEMU monitor socket at {}",
                        socket_path.display()
                    )
                })
            }
        }
    }
}

fn wait_for_file(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            bail!(
                "timed out after {timeout:?} waiting for {} to be created",
                path.display()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

/// `bootloader` パッケージを UEFI ターゲット向けにビルドし、生成された
/// `.efi` バイナリのパスを返す。`panic_test` が true の場合、起動完了直後に
/// 意図的に `panic!` する `panic-test` フィーチャを有効にする。
fn build_bootloader(workspace_root: &Path, panic_test: bool) -> Result<PathBuf> {
    let mut args = vec![
        "build",
        "--target",
        UEFI_TARGET,
        "-p",
        BOOTLOADER_PACKAGE,
        "--bin",
        BOOTLOADER_PACKAGE,
    ];
    if panic_test {
        args.push("--features");
        args.push(PANIC_TEST_FEATURE);
    }

    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .args(&args)
        .status()
        .context("failed to invoke cargo to build the bootloader")?;

    if !status.success() {
        bail!("bootloader build failed ({status})");
    }

    let efi_path = workspace_root
        .join("target")
        .join(UEFI_TARGET)
        .join("debug")
        .join(format!("{BOOTLOADER_PACKAGE}.efi"));
    if !efi_path.exists() {
        bail!(
            "bootloader build reported success but {} is missing",
            efi_path.display()
        );
    }
    Ok(efi_path)
}

/// `kernel` パッケージを `x86_64-unknown-none` ターゲット向けにビルドし、
/// 生成された ELF バイナリのパスを返す。
/// クリティカルセクション/ロックの回帰チェックを 1 種類実行する。
///
/// 例外テストと同じく **TCG 固定**。
/// `--interrupt-test keyboard` が QEMU monitor へ流すキー列。
///
/// `codes` は、そのキーで i8042 が出すスキャンコードの本数。修飾つきは
/// 「修飾の押下・本体の押下・本体の離脱・修飾の離脱」で 4 本、単独は
/// 押下と離脱で 2 本になる。取りこぼしの検証に使うため、期待値を
/// ここで明示的に持つ。
struct KeyInjection {
    monitor: &'static str,
    codes: u64,
}

/// 送るキー列。結果は `Hello!\\` になる。
///
/// **大文字と記号の両方を含める。** どちらも Shift を伴い、英字は
/// Shift と Caps の XOR、記号は Shift のみという非対称な経路を通る。
///
/// # 末尾の 2 つは変換表の外に居る（zi-e）
///
/// **`ro`（`0x73`）と `yen`（`0x7D`）はどちらも `\` を出す**ので、行は
/// `Hello!\\` で終わる。**表の外に居るものは、範囲の判定より先に引く経路が
/// 要る**——**ホストの単体テストが固定するのは表までで、打鍵が実機の消費者
/// まで届くことは言えない**（`kernel/src/keyboard/decode.rs` のモジュール doc）。
///
/// **ここが見るのはカーネル側の消費者である。** **Ring 3 の前景経路は
/// `--shell-test` が見る**——**前景が取られている間、こちらの消費者は
/// 1 バイトも取り出さない**ので、片方では両方を主張できない。
const KEYBOARD_TEST_KEYS: &[KeyInjection] = &[
    KeyInjection {
        monitor: "shift-h",
        codes: 4,
    },
    KeyInjection {
        monitor: "e",
        codes: 2,
    },
    KeyInjection {
        monitor: "l",
        codes: 2,
    },
    KeyInjection {
        monitor: "l",
        codes: 2,
    },
    KeyInjection {
        monitor: "o",
        codes: 2,
    },
    KeyInjection {
        monitor: "shift-1",
        codes: 4,
    },
    KeyInjection {
        monitor: "ro",
        codes: 2,
    },
    KeyInjection {
        monitor: "yen",
        codes: 2,
    },
    KeyInjection {
        monitor: "ret",
        codes: 2,
    },
];

/// 期待する 1 行。
const KEYBOARD_TEST_EXPECTED_LINE: &str = "keyboard: line = \"Hello!\\\\\"";

/// キーボードの回帰チェック（`--interrupt-test keyboard`）。
///
/// QEMU monitor の `sendkey` で既知のキー列を注入し、シリアルログを検証する。
/// monitor への接続は `screenshot` の実装を再利用する。
///
/// **キーリピート（タイプマティック）は検証できない。** `sendkey` は保持時間を
/// 指定してもリピートを模擬せず、押下と離脱を 1 組送るだけである
/// （`sendkey a 3000` で実測）。リピートは手動確認に回す。
/// キーボードの回帰チェックの個別主張。**破壊確認が「どれが落ちたか」を
/// 見るために、合否をまとめずに返す。**
#[derive(Debug, Clone, Copy)]
struct KeyboardAssertions {
    /// 期待した文字列になったか。
    line: bool,
    /// 新しいベクタ（0x42）で届いたか。**到達の検出経路。**
    arrived_on_new_vector: bool,
    /// 旧ベクタ（0x21）で届いたキーが無いか。**二重配送の検出。**
    no_legacy_delivery: bool,
    /// redirection entry のベクタ欄が書いた値か。**設定の検出経路。**
    readback_vector: bool,
    /// redirection entry のマスクが外れているか。**解禁の検出経路。**
    readback_unmasked: bool,
    /// 送った本数と受け取った本数が一致するか。
    count: bool,
    /// 会計が閉じているか。
    balanced: bool,
    /// `sti` 前の検査が落ちて、割り込みを有効にせず止まったか。
    ///
    /// **正常な起動では `false` である。** 破壊確認で「何も届かない」だけを
    /// 見ると、原因を問わず通ってしまう。**どこで止まったかを名指しする。**
    refused_sti: bool,
}

impl KeyboardAssertions {
    fn all_ok(&self) -> bool {
        self.line
            && self.arrived_on_new_vector
            && self.no_legacy_delivery
            && self.readback_vector
            && self.readback_unmasked
            && self.count
            && self.balanced
            && !self.refused_sti
    }
}

/// 複製した ext2 の像をホストへ取り出し、元の像と突き合わせる（S12-a）。
///
/// # 何を主張するか。**経路であって、書き込みではない**
///
/// **S12-a は像を書き換えない。** したがって取り出した像は、
/// `build.rs` が建てた像と**バイト単位で一致するはずである。**
/// **主張は「複製と取り出しが 1 バイトも落とさないこと」である。**
///
/// # 判定は 3 本ある
///
/// - **複製先がカーネル像の外にあること。** これが無いと「複製した」が
///   反証できない——**複製せずに `.rodata` の番地を出す形が通ってしまう**
/// - **取り出した像が、建てた像とバイト単位で一致すること。** これが要である
/// - **`e2fsck` が無傷と判定すること**
///
/// # 3 本目は、いまは何も新しく検査していない
///
/// **バイト一致が真である限り、`e2fsck` は絶対に落ちない**——
/// S10-a が既に建てた像へ `e2fsck` を当てており（`--full` の `fs image e2fsck`）、
/// **同じバイト列に同じ道具を当てているからである。含意される。**
///
/// **それでも置く。S12-b 以降で使う経路の予行だからである。**
/// **書き換えが入った瞬間に、こちらが主たる判定になる**——
/// そのとき「バイト一致」は偽になるのが正常で、**含意が成り立たなくなる。**
/// **落ちないと分かっていて置いていると、ここに書いておく。**
///
/// # `pmemsave` である（`memsave` ではない）
///
/// **実測で確かめた。** monitor の `help` はこう答える。
///
///     memsave  addr size file -- save to disk virtual memory dump ...
///     pmemsave addr size file -- save to disk physical memory dump ...
///
/// **カーネルが出すのは物理アドレスなので `pmemsave` である。**
fn cmd_fs_image_extract(features: &[&str]) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    // **構成ごとに別のログへ書く。** 落ちたときに、どの構成のものかが残る。
    let tag = if features.is_empty() {
        "default".to_string()
    } else {
        features.join("-")
    };
    let serial_log = workspace_root
        .join("target")
        .join(format!("fs-extract-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);
    let dump = workspace_root
        .join("target")
        .join(format!("fs-extract-{tag}.img"));
    let _ = fs::remove_file(&dump);
    let monitor_socket = PathBuf::from(format!(
        "/tmp/zaytos-xtask-fsextract-{}.sock",
        std::process::id()
    ));
    let _ = fs::remove_file(&monitor_socket);
    ensure_socket_path_fits(&monitor_socket)?;

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: Some(&monitor_socket),
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the fs image extraction")?;

    // **複製の行が出るまで待つ。上限つき。**
    // **取り出す合図。** **複製した直後ではなく、像の作業が終わった時点である。**
    // **`fs-image-copy` を合図にすると、割り当て・追記・縮めの途中で取り出しうる**
    // （実測で踏んだ）。
    let ready_marker = "fs-image-ready:";
    let marker = "fs-image-copy: copied ";
    let source_marker = "fs-image-source: root_filesystem reads from ";
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let mut copied_line = None;
    while Instant::now() < deadline {
        {
            let text = read_lossy(&serial_log);
            // **完了の行を待ってから、複製の行を拾う。**
            if text.contains(ready_marker) {
                if let Some(line) = text.lines().find(|l| l.contains(marker)) {
                    copied_line = Some(line.to_string());
                    break;
                }
            }
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    // **割り当てたままの像を取り出す構成か。**
    // **判定 1（往復のバイト一致）と判定 2（`e2fsck` の不満が 1 本）は、
    // 像の状態が違うので同じ起動では両方言えない。** 構成で分ける。
    let keep_allocated = features.contains(&KEEP_ALLOCATED_FEATURE);
    // **追記したままの像か（S12-c）。** 判定 A・B・D はこの構成でしか言えない。
    let keep_written = features.contains(&WRITE_KEEP_FEATURE);

    let context = if features.is_empty() {
        "fs-extract".to_string()
    } else {
        format!("fs-extract {}", features.join("+"))
    };
    let context = context.as_str();

    let mut extracted = false;
    if let Some(line) = &copied_line {
        if let Some((base, end)) = parse_copy_range(line) {
            match connect_monitor_with_retry(&monitor_socket) {
                Ok(mut stream) => {
                    // **物理メモリを取り出す。** 範囲はカーネルが出した値そのままで、
                    // xtask は長さの定数を持たない（**像の大きさは変わりうる**）。
                    let size = end - base;
                    // **ファイル名を引用符で囲む。** 囲まないと、**サイズの式が
                    // パスの `/` を除算として飲み込む**——実測で
                    // `invalid char 't' in expression` になった（`/tmp/...` の `t`）。
                    let command = format!("pmemsave {base:#x} {size:#x} \"{}\"\n", dump.display());
                    if stream.write_all(command.as_bytes()).is_ok() {
                        // 書き終わるのを待つ。上限つき。
                        let deadline = Instant::now() + Duration::from_secs(30);
                        while Instant::now() < deadline {
                            if fs::metadata(&dump).map(|m| m.len()).unwrap_or(0) == size {
                                extracted = true;
                                break;
                            }
                            thread::sleep(PANIC_TEST_POLL_INTERVAL);
                        }
                    }
                }
                Err(e) => println!("{context}: could not reach the QEMU monitor: {e}"),
            }
        }
    }

    // **装置が実際に読まれた量（S13-c）。** QEMU の帳簿を、殺す前に聞く。
    // **下限で見る**——OVMF の起動時の探りが混ざる（実測 8,704 バイト。像の
    // 0.4%）ので差分は取らず、「像 1 枚ぶん以上」を要求する。基線を 2 回聞く
    // 形は採らない——カーネルは xtask を待たないので、時機の競争になる。
    // 読んだ量と**書いた量**（S13-c / S13-e）を、1 回の帳簿から取る。
    let (disk_rd_bytes, disk_wr_bytes) = match connect_monitor_with_retry(&monitor_socket) {
        Ok(mut stream) => match query_monitor(&mut stream, "info blockstats").ok() {
            Some(text) => (parse_disk0_rd_bytes(&text), parse_disk0_wr_bytes(&text)),
            None => (None, None),
        },
        Err(_) => (None, None),
    };

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&monitor_socket);

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    let Some(line) = copied_line else {
        bail!("{context}: the kernel never reported {marker:?}");
    };
    println!("{context}: {}", line.trim());

    // **1 本目——複製先がカーネル像の外にあること。**
    let outside_kernel_image = match (parse_copy_range(&line), parse_kernel_image_range(&line)) {
        (Some((base, end)), Some((image_start, image_end))) => {
            end <= image_start || base >= image_end
        }
        _ => false,
    };
    println!("{context}: the copy lies outside the kernel image = {outside_kernel_image}");

    // **読む側が複製を見ていること（S12-b の 1 段目）。**
    //
    // **複製は元の像とバイト単位で一致しているので、向けても向けなくても
    // 読めるものは変わらない。番地だけが違う。** したがって
    // **番地を突き合わせないと「向けた」ことを主張できない。**
    let reader_phys = serial
        .lines()
        .find(|l| l.contains(source_marker))
        .and_then(parse_reader_phys);
    let reads_the_copy = match (reader_phys, parse_copy_range(&line)) {
        (Some(reader), Some((base, _))) => reader == base,
        _ => false,
    };
    println!("{context}: root_filesystem reads from the copy = {reads_the_copy}");

    // **カーネルが読んだ空き数が、外の道具の値と一致すること（S12-b の 2 段目）。**
    //
    // **自分の解析を自分で確かめても、欄の位置を取り違えていれば気づけない。**
    // **`dumpe2fs` は同じ像について独立した答えを持っている**ので、
    // **突き合わせる相手にする**（`e2fsck` と同じ `e2fsprogs` にあり、道具は増えない）。
    //
    // **期待値を定数で持たない。** 像が変われば空き数も変わるので、
    // **そのつど外の道具から取る。**
    // **像は、いま建てた構成のものを見る（ES-d の同族の洗い出し）。**
    // **`kernel_build_out_dir` は `--features` を付けずに建て直す**ので、
    // **構成を変えても既定の像が返る。** ここは破壊 feature つきで呼ばれるので、
    // **像を変える feature が来たら期待値が別の像から来ることになる**
    // （`stage_esp` で実際に起きた形である。`docs/troubleshooting.md`）。
    // **いまその形の feature はここへ来ないが、来たときに静かに壊れる。**
    let built = kernel_elf.out_dir.join(FS_IMAGE_NAME);
    let expected_counts = dumpe2fs_free_counts(&built)?;
    let kernel_counts = parse_kernel_free_counts(&serial);
    let free_counts_agree = kernel_counts
        .as_ref()
        .is_some_and(|counts| *counts == expected_counts);
    println!(
        "{context}: the free counts match dumpe2fs = {free_counts_agree} (kernel {:?}, dumpe2fs \
         {expected_counts:?})",
        kernel_counts
    );

    // **像の状態で判定が分かれる。**
    println!("{context}: e2fsck version = {}", e2fsck_version());
    let complaints = if extracted {
        e2fsck_complaint_lines(&dump)?
    } else {
        std::vec!["the image was not extracted".to_string()]
    };

    // **0 まで縮めたままの像か（S12-d）。**
    let keep_truncated = features.contains(&TRUNCATE_KEEP_FEATURE);
    // **作ったままの像か（S12-e）。**
    let keep_created = features.contains(&CREATE_KEEP_FEATURE);
    let keep_made = features.contains(&MKDIR_KEEP_FEATURE);

    let (identical, fsck_ok) = if keep_truncated {
        // **空のファイルは ext2 として正しい。** **`e2fsck` は無傷と判定する。**
        let clean = extracted && complaints.is_empty();
        println!(
            "{context}: e2fsck found nothing to complain about = {clean} (complaints: \
             {complaints:?})"
        );

        // **長さが 0 であること。** **外の道具に読ませる**（自分で言わない）。
        // **「引けなかった」を「空」と混ぜない**（S12-e で締めた。
        // `debugfs_read` の doc に理由がある）。
        let actual = debugfs_read(&dump, "/data/writable")?;
        let emptied = actual.as_ref().is_some_and(|bytes| bytes.is_empty());
        println!(
            "{context}: the file reads back empty = {emptied} ({:?} byte(s))",
            actual.as_ref().map(|bytes| bytes.len())
        );

        // **持っていたブロックが返っていること。**
        // **1 ブロックのファイルを空にしたので、空き数は 1 つ増える。**
        let after = dumpe2fs_free_counts(&dump)?;
        let returned = after.superblock_blocks == expected_counts.superblock_blocks + 1;
        println!(
            "{context}: the block it held came back = {returned} (built {}, extracted {})",
            expected_counts.superblock_blocks, after.superblock_blocks
        );

        (emptied && returned, clean)
    } else if keep_created {
        // **作ったままの像（S12-e）。** **判定 A・B・D はこの構成でしか言えない。**
        //
        // **判定A——`e2fsck` の不満が 0 本。**
        let clean = extracted && complaints.is_empty();
        println!(
            "{context}: e2fsck found nothing to complain about = {clean} (complaints: \
             {complaints:?})"
        );

        // **判定B——作ったファイルが引けて、中身も長さも一致すること。**
        let expected = expected_created_content();
        let actual = debugfs_read(&dump, CREATED_PATH)?;
        let content_ok = actual.as_deref() == Some(expected.as_slice());
        println!(
            "{context}: {CREATED_PATH} reads back exactly = {content_ok} (expected {} byte(s), \
             got {:?} byte(s))",
            expected.len(),
            actual.as_ref().map(|bytes| bytes.len())
        );

        // **判定D——空きブロックと空き inode が 1 つずつ減っていること。**
        // **inode の空き数を判定に使うのは、ここが初めてである。**
        //
        // **`bg_used_dirs_count` も一緒に見る。** **ファイルの作成では動かない**
        // （実測。`debugfs` でファイルを作ると変わらず、ディレクトリを作ると 1 増えた）。
        let after = dumpe2fs_free_counts(&dump)?;
        let moved_by_one = after.superblock_blocks + 1 == expected_counts.superblock_blocks
            && after.superblock_inodes + 1 == expected_counts.superblock_inodes
            && after.group_inodes + 1 == expected_counts.group_inodes
            && after.group_dirs == expected_counts.group_dirs;
        println!(
            "{context}: one block and one inode went away, the directory count did not = \
             {moved_by_one} (built {:?}, extracted {after:?})",
            expected_counts
        );

        // **判定E——作った inode が、像が望むとおりに追加領域を名乗ること（S12-f-3）。**
        //
        // **参照は 2 つ要る。**
        //
        // - **`mke2fs` が作った inode と一致すること**（同じ像の中で揃っていること）
        // - **`dumpe2fs` の `Desired extra isize` と一致すること**
        //
        // **後者が無いと、参照側が 0 の像で判定が空振りする**——
        // **128 バイト inode の像になれば両方 0 で通り、破壊（名乗らない）も通る。**
        // **族の1つ目そのものである**（実際、ホストの単体テストで踏んだ。
        // テスト像の inode が欄を持っておらず、両方 0 で通っていた）。
        //
        // **望む値が 0 か読めない像では、黙って通さずに落とす**——
        // **「適用外」を緑にすると、適用外になったことに誰も気づかない。**
        let desired = dumpe2fs_desired_extra_isize(&built)?;
        let ours = debugfs_extra_isize(&dump, CREATED_PATH)?;
        let theirs = debugfs_extra_isize(&dump, "/data/writable")?;
        let extra_ok = match desired {
            Some(want) if want != 0 => ours == Some(want) && theirs == Some(want),
            _ => false,
        };
        println!(
            "{context}: the new inode names the extra area the way the image asks = {extra_ok} \
             (desired {desired:?}, ours {ours:?}, mke2fs's {theirs:?}; a desired of 0 or None \
             makes this check inapplicable, and inapplicable is not a pass)"
        );

        (content_ok && moved_by_one && extra_ok, clean)
    } else if keep_made {
        // **作ったままのディレクトリ（DIR-1c）。**
        //
        // **判定A——`e2fsck` の不満が 0 本。**
        //
        // **3 つの破壊はここで捕まる**——`..` が無い / 親の参照数が上がって
        // いない / 群のディレクトリ数が古い。**どれも「読めるか」では
        // 分からず、外の道具に訊くしかない。**
        //
        // **`..` と親の参照数について、こちらは独立の判定を持っていない。**
        // **`e2fsck` の文言に乗っている**（`E2FSCK_NOISE` に当たらない行を
        // 不満として数える形）。**観測していないことは観測していないと書く。**
        let clean = extracted && complaints.is_empty();
        println!(
            "{context}: e2fsck found nothing to complain about = {clean} (complaints: \
             {complaints:?})"
        );

        // **判定B——空きブロックと空き inode が 1 つずつ減り、
        // **群のディレクトリ数が 1 増えていること。**
        //
        // **ファイルを作ったときと違うのはここである**——**あちらは
        // ディレクトリ数が動かないことを主張していた**（同じ欄を、
        // 逆向きに使っている）。
        let after = dumpe2fs_free_counts(&dump)?;
        let counts_moved = after.superblock_blocks + 1 == expected_counts.superblock_blocks
            && after.superblock_inodes + 1 == expected_counts.superblock_inodes
            && after.group_inodes + 1 == expected_counts.group_inodes
            && after.group_dirs == expected_counts.group_dirs + 1;
        println!(
            "{context}: one block and one inode went away and the directory count rose by one = \
             {counts_moved} (built {:?}, extracted {after:?})",
            expected_counts
        );

        (counts_moved, clean)
    } else if keep_written {
        // **追記したままの像（S12-c）。** **中身が inode から参照され、会計が
        // 締まっているので、`e2fsck` は不満を 1 本も言わないはずである**（実測）。
        let clean = extracted && complaints.is_empty();
        println!(
            "{context}: e2fsck found nothing to complain about = {clean} (complaints: \
             {complaints:?})"
        );

        // **判定B——中身と長さの両方が一致すること。**
        // **「含む」で見ない**——前後に余分が無いことを言う。
        let expected = expected_writable_content();
        let actual = debugfs_read(&dump, "/data/writable")?;
        let content_ok = actual.as_deref() == Some(expected.as_slice());
        println!(
            "{context}: the appended bytes read back exactly = {content_ok} (expected {} byte(s), \
             got {:?} byte(s))",
            expected.len(),
            actual.as_ref().map(|bytes| bytes.len())
        );

        // **判定D——空き数の減りがちょうど 1 つ。**
        // **追記 1 は末尾の空きを埋めるだけで割り当てを起こさない**ので、
        // **2 回の追記で減るのは 1 つだけである。** 2 つ減っていれば、
        // **1 回目でも割り当てている。**
        let after = dumpe2fs_free_counts(&dump)?;
        let moved_by_one = after.superblock_blocks + 1 == expected_counts.superblock_blocks;
        println!(
            "{context}: the free blocks dropped by exactly one across both appends = \
             {moved_by_one} (built {}, extracted {})",
            expected_counts.superblock_blocks, after.superblock_blocks
        );

        (content_ok && moved_by_one, clean)
    } else if keep_allocated {
        // **割り当てたままの像。** **`e2fsck` は必ず 1 本だけ不満を言う**——
        // **どの inode も参照していないブロックに使用中の印が立っている**からで、
        // **会計が正しければそれ以外は出ない**（実測）。
        //
        // **強い形で見る**——**`Block bitmap differences` の 1 本だけで、
        // それ以外の不満が無いこと。** 想定外の行が出たら落とす。
        let only_bitmap =
            complaints.len() == 1 && complaints[0].starts_with("Block bitmap differences:");
        println!(
            "{context}: e2fsck complains exactly once, about the bitmap = {only_bitmap} \
             (complaints: {complaints:?})"
        );

        // **空き数が 1 つ減っていること。外の道具から読む**——
        // **自分で「減らした」と言わない**（段(2) で `dumpe2fs` へ寄せたのと同じ理由）。
        let after = dumpe2fs_free_counts(&dump)?;
        let moved_by_one = after.superblock_blocks + 1 == expected_counts.superblock_blocks
            && after.group_blocks + 1 == expected_counts.group_blocks;
        println!(
            "{context}: the free counts dropped by exactly one = {moved_by_one} (built \
             {}/{}, extracted {}/{})",
            expected_counts.superblock_blocks,
            expected_counts.group_blocks,
            after.superblock_blocks,
            after.group_blocks
        );
        (moved_by_one, only_bitmap)
    } else {
        // **往復した後の像。** **1 バイトも違わないはずである。**
        let identical = match (fs::read(&dump), fs::read(&built)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        println!(
            "{context}: the extracted image matches the built image byte for byte = {identical}"
        );

        // **判定C——作って消したファイルが、もう引けないこと（S12-e）。**
        //
        // **バイト一致が真なら含意される。** S12-a の 3 本目と同じ位置づけで、
        // **落ちないと分かっていて置いている**——**バイト一致のほうが強いので、
        // これが単独で落ちることは無い。** それでも置くのは、
        // **「消えたこと」が判定行として読めるようにするためである。**
        let gone = extracted && debugfs_read(&dump, CREATED_PATH)?.is_none();
        println!("{context}: {CREATED_PATH} is no longer there = {gone}");

        // **不満が 1 本も無いこと。** **バイト一致が真ならこれは含意される**が、
        // **S12-c で書き換えたまま残す段になると、こちらが主たる判定になる。**
        let clean = extracted && complaints.is_empty();
        println!("{context}: e2fsck found nothing to complain about = {clean} (complaints: {complaints:?})");
        (identical && gone, clean)
    };

    // **装置が像 1 枚ぶん以上を配ったこと（S13-c）。** バイト一致は
    // 「複製の中身が正しい」ことしか言えない——**複製元が装置だったことは、
    // QEMU の帳簿だけが独立に言える**（`fs-load-from-embedded` の破壊は
    // 中身が同一なので、ここでしか捕まらない）。期待値はホスト側の像の
    // ファイルの長さからそのつど導く。
    let image_bytes = fs::metadata(&built).map(|m| m.len()).unwrap_or(0);
    let device_read_whole_image =
        disk_rd_bytes.is_some_and(|read| image_bytes > 0 && read >= image_bytes);
    println!(
        "{context}: the virtio disk delivered {disk_rd_bytes:?} byte(s); the image is \
         {image_bytes} byte(s); at least the whole image came from the device = \
         {device_read_whole_image}"
    );

    // === S13-e: 書き戻し（flush）の判定 ===
    //
    // **2 系統である**（読み側と対称）。**(1) 帳簿の下限**——装置が像 1 枚ぶん
    // 以上を書いたこと。既定でも「書いた」を言える（内容が変わらなくても）。
    // **(2) 装置の中身**——`disk0.img`（装置が書いた結果）と `dump`（RAM 複製を
    // pmemsave したもの）がバイト一致すること。**pmemsave 系統は RAM 複製の
    // 正しさを、この系統は「装置に届いた結果」を見る。** keep 変種では両者が
    // 「書いたまま」の像で一致し、`fs-flush-skip` は装置が古いままなので
    // 食い違う（S13-c の取り違えと対の形）。
    let device_wrote_whole_image =
        disk_wr_bytes.is_some_and(|wrote| image_bytes > 0 && wrote >= image_bytes);
    println!(
        "{context}: the virtio disk received {disk_wr_bytes:?} byte(s); at least the whole \
         image went to the device = {device_wrote_whole_image}"
    );

    let disk_matches_dump = if extracted {
        match (fs::read(&dump), fs::read(disk_image_path(&esp_dir))) {
            (Ok(ram), Ok(disk)) => ram == disk,
            _ => false,
        }
    } else {
        false
    };
    println!(
        "{context}: the disk image the device wrote matches the RAM copy byte for byte = \
         {disk_matches_dump} (independent of pmemsave; e2fsck on disk0.img would agree)"
    );

    // **カーネルが出した像の検査値を、ホストが独立に計算した値と突き合わせる（P-e）。**
    //
    // **埋め込み像を外したので、カーネル側の「バイト一致」は無くなった**
    // （`ADR-0034` の Addendum）。**代わりがこれである。**
    // **源が独立である**——**ホストは建てた像のファイルを直に読み、
    // カーネルは virtio を通って読んだ複製を見ている。**
    //
    // **突き合わせる相手は「起動の時点で装置に在ったもの」である。**
    // **この経路では `stage_esp` が建てた像をそのまま置くので、建てた像が
    // その中身である。** **持ち越す経路（P-a）では相手が変わる。**
    let kernel_image_checksum = serial
        .lines()
        .find(|line| line.contains("fs-image-copy: copied"))
        // **`parse_marked_hex` を使わない。** **あれは空白で区切るが、この行では
        // 値の直後が `;` である**——**実測で `None` になった**（2026-08-28）。
        .and_then(|line| {
            let rest = line.split("checksum=").nth(1)?;
            let token = rest.split(|c: char| c == ';' || c.is_whitespace()).next()?;
            u32::from_str_radix(token.trim_start_matches("0x"), 16).ok()
        });
    let host_image_checksum = fs::read(&built).ok().map(|bytes| image_checksum(&bytes));
    let image_checksum_matches =
        kernel_image_checksum.is_some() && kernel_image_checksum == host_image_checksum;
    println!(
        "{context}: the kernel's image checksum matches the host's = {image_checksum_matches} \
         (kernel {kernel_image_checksum:?}, host {host_image_checksum:?})"
    );

    if outside_kernel_image
        && reads_the_copy
        && free_counts_agree
        && image_checksum_matches
        && identical
        && fsck_ok
        && device_read_whole_image
        && device_wrote_whole_image
        && disk_matches_dump
    {
        println!("{context}: PASS");
        Ok(())
    } else {
        // **文言に結合しているので、落ちた理由の切り分けを 1 行出す。**
        // **「実装が壊れた」と「文言が変わった」を、読む人が最初に分けられるように。**
        println!(
            "{context}: note - the judgements above read e2fsck's wording. If the complaints look \
             unfamiliar, check whether e2fsck changed version (see the version line above); the \
             known-quiet lines are listed in E2FSCK_NOISE in xtask"
        );
        bail!("{context}: FAILED")
    }
}

/// ビットマップの破壊と、それぞれが要る構成（S12-b）。
///
/// **3 つは割り当て中の像で見る**ので `fs-alloc-keep-test` と組む。
/// **`free-skip-bit` だけは往復で見る**ので既定の構成である
/// （**あれは解放の側を壊すので、解放を飛ばす構成では現れない**）。
const FS_BITMAP_SABOTAGES: &[(&str, &[&str])] = &[
    (
        "a superblock count left stale",
        &[KEEP_ALLOCATED_FEATURE, "ext2-alloc-skip-sb-count-test"],
    ),
    (
        "a group count left stale",
        &[KEEP_ALLOCATED_FEATURE, "ext2-alloc-skip-bg-count-test"],
    ),
    (
        "allocating a block that is already in use",
        &[KEEP_ALLOCATED_FEATURE, "ext2-alloc-ignore-bitmap-test"],
    ),
    (
        "freeing without clearing the bit",
        &["ext2-free-skip-bit-test"],
    ),
];

/// 0 まで縮めてそのままにする構成の feature 名（S12-d）。**変種であって破壊ではない。**
const TRUNCATE_KEEP_FEATURE: &str = "fs-truncate-keep-test";

/// 縮める破壊（S12-d）。**6 つとも既定の構成（往復）で見る。**
///
/// **往復のバイト一致が要である**——**`e2fsck` は 4 つを無傷と判定する。**
/// 返し過ぎ・返さなさ過ぎ・切った先の埋め損ねは、**像には残るが
/// ext2 として不整合ではない**（使われていないブロックの中身は自由である）。
const FS_TRUNCATE_SABOTAGES: &[(&str, &[&str])] = &[
    (
        "freeing one block too many",
        &["ext2-truncate-off-by-one-test"],
    ),
    (
        "freeing a block when none should be returned",
        &["ext2-truncate-always-free-test"],
    ),
    (
        "leaving the freed block in the inode",
        &["ext2-truncate-keep-slot-test"],
    ),
    (
        "shrinking without freeing",
        &["ext2-truncate-skip-free-test"],
    ),
    (
        "leaving the bytes past the new end",
        &["ext2-truncate-keep-tail-test"],
    ),
    ("a stale i_blocks", &["ext2-truncate-skip-blocks-test"]),
];

/// 追記の破壊と、それぞれが要る構成（S12-c）。**6 つとも書いたままの像で見る。**
///
/// **`round-size` だけは判定 A を通り抜ける**——`e2fsck` が期待する値そのものを
/// 書くので不満が出ない。**判定 B（中身と長さ）だけが落ちる。**
/// **判定 B が独立に効いていることの反証である。**
const FS_WRITE_SABOTAGES: &[(&str, &[&str])] = &[
    (
        "a stale i_size",
        &[WRITE_KEEP_FEATURE, "ext2-append-skip-size-test"],
    ),
    (
        "an i_size rounded up to the block boundary",
        &[WRITE_KEEP_FEATURE, "ext2-append-round-size-test"],
    ),
    (
        "a stale i_blocks",
        &[WRITE_KEEP_FEATURE, "ext2-append-skip-blocks-test"],
    ),
    (
        "i_blocks written in bytes",
        &[WRITE_KEEP_FEATURE, "ext2-append-blocks-in-bytes-test"],
    ),
    (
        "a block that is never linked into the inode",
        &[WRITE_KEEP_FEATURE, "ext2-append-skip-link-test"],
    ),
    (
        "allocating without using the space in the last block",
        &[WRITE_KEEP_FEATURE, "ext2-append-always-allocate-test"],
    ),
];

/// 追記したままにする構成の feature 名（S12-c）。**破壊ではなく変種である。**
const WRITE_KEEP_FEATURE: &str = "fs-write-keep-test";

/// `/data/writable` の初期の中身と、カーネルが足す量（S12-c）。
///
/// **`build.rs` とカーネルが同じ規則で埋めている**——位置から決まる形にしてある
/// （定数の並びだと、書けていない箇所と元から同じ箇所が見分けにくい）。
/// **ここは同じ規則で期待値を組み立てるだけで、値を書き写さない。**
const WRITABLE_SEED_BYTES: usize = 100;
const WRITABLE_APPENDED_BYTES: usize = 200 + 4000;

/// 追記した後の `/data/writable` の中身。
fn expected_writable_content() -> Vec<u8> {
    let pattern = |len: usize| -> Vec<u8> { (0..len).map(|i| (i % 251) as u8).collect() };
    let mut expected = pattern(WRITABLE_SEED_BYTES);
    expected.extend(pattern(WRITABLE_APPENDED_BYTES));
    expected
}

/// カーネルが作るファイルの名前と中身の長さ（S12-e）。
///
/// **`WRITABLE_*` と同じ作法である**——**同じ規則で組み立てるだけで、
/// 値を書き写さない。**
const CREATED_PATH: &str = "/data/created";
const CREATED_BYTES: usize = 300;

/// 作った後の `/data/created` の中身。
fn expected_created_content() -> Vec<u8> {
    (0..CREATED_BYTES).map(|i| (i % 251) as u8).collect()
}

/// 作ったままにする構成の feature 名（S12-e）。**破壊ではなく変種である。**
const CREATE_KEEP_FEATURE: &str = "fs-create-keep-test";

/// 作成と削除の破壊（S12-e）。
///
/// **5 つは作ったままの像で見る**ので `fs-create-keep-test` と組む。
/// **`unlink-mark-unused` だけは往復で見る**ので既定の構成である
/// （**あれは削除の側を壊すので、消さない構成では現れない**——
/// S12-b の `free-skip-bit` と同じ形）。
///
/// **`unlink-mark-unused` は `e2fsck` を通り抜ける。**
/// `inode = 0` の枠を残す形は **ext2 として不整合ではない**ので、
/// **往復のバイト一致だけが捕まえる**（S12-d の 4 つと同じ機序である）。
/// ディレクトリを作ったままにする構成の feature 名（DIR-1c）。**変種である。**
const MKDIR_KEEP_FEATURE: &str = "fs-mkdir-keep-test";

/// ディレクトリの作成と削除の破壊（DIR-1c）。
///
/// # 3 つは作ったままの像で見る
///
/// **`.` と `..`・親の `i_links_count`・群の `bg_used_dirs_count` は、
/// 消してしまうと現れない**（`fs-create-keep-test` と同じ形）。
///
/// # `rmdir-ignore-nonempty` は既定の構成である
///
/// **あれは消す側を壊すので、消さない構成では通らない。**
/// **捕まえるのはカーネル側の判定行である**——**空でない `rmdir` が
/// 断られなければ、起動が止まる**（`kernel/src/main.rs` の
/// `MkdirRemovedNonEmpty`）。
const FS_MKDIR_SABOTAGES: &[(&str, &[&str])] = &[
    (
        "a new directory without its .. entry",
        &[MKDIR_KEEP_FEATURE, "ext2-mkdir-skip-dot-dot-test"],
    ),
    (
        "a parent whose link count was not raised",
        &[MKDIR_KEEP_FEATURE, "ext2-mkdir-skip-parent-link-test"],
    ),
    (
        "a group directory count left stale",
        &[MKDIR_KEEP_FEATURE, "ext2-mkdir-skip-dirs-count-test"],
    ),
    (
        "rmdir removing a directory that is not empty",
        &["ext2-rmdir-ignore-nonempty-test"],
    ),
];

const FS_CREATE_SABOTAGES: &[(&str, &[&str])] = &[
    (
        "an inode handed out without marking the bitmap",
        &[CREATE_KEEP_FEATURE, "ext2-create-skip-inode-bit-test"],
    ),
    (
        "a link count left at zero",
        &[CREATE_KEEP_FEATURE, "ext2-create-skip-links-test"],
    ),
    (
        "a previous entry whose record length is not shrunk",
        &[CREATE_KEEP_FEATURE, "ext2-create-keep-prev-rec-len-test"],
    ),
    (
        "free inode counts left stale",
        &[CREATE_KEEP_FEATURE, "ext2-create-skip-inode-count-test"],
    ),
    (
        "a directory count moved for a plain file",
        &[CREATE_KEEP_FEATURE, "ext2-create-move-dirs-count-test"],
    ),
    (
        "unlinking that leaves the slot behind instead of merging it",
        &["ext2-unlink-mark-unused-test"],
    ),
    (
        "a new inode that does not name its extra area",
        &[CREATE_KEEP_FEATURE, "ext2-create-skip-extra-isize-test"],
    ),
];

/// ある像のあるパスの `i_extra_isize` を、`debugfs` に読ませる（S12-f-3）。
///
/// **`debugfs` は `stat` の末尾に `Size of extra inode fields: N` を出す**（実測）。
/// **自分で inode の位置を算術して読まない**——**書く側と同じ算術を判定でも書くと、
/// 取り違えが両側で相殺する。**
fn debugfs_extra_isize(image: &Path, path: &str) -> Result<Option<u64>> {
    let output = external_tool("debugfs")
        .env("LC_ALL", "C")
        .arg("-R")
        .arg(format!("stat {path}"))
        .arg(image)
        .output()
        .context("failed to invoke debugfs (it ships with e2fsprogs)")?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Size of extra inode fields:"))
        .and_then(|value| value.trim().parse().ok()))
}

/// 像が新しい inode に望む `i_extra_isize`（S12-f-3）。
///
/// **`dumpe2fs` の `Desired extra isize` である**（`s_want_extra_isize`。実測で確かめた）。
/// **32 という数を判定に書かないためにここから取る**——**像が変われば動く値である。**
fn dumpe2fs_desired_extra_isize(image: &Path) -> Result<Option<u64>> {
    let output = external_tool("dumpe2fs")
        .env("LC_ALL", "C")
        .arg("-h")
        .arg(image)
        .output()
        .context("failed to invoke dumpe2fs (it ships with e2fsprogs)")?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .find_map(|line| line.strip_prefix("Desired extra isize:"))
        .and_then(|value| value.trim().parse().ok()))
}

/// 取り出した像からファイルの中身を読む（S12-c。S12-e で名前を引数にした）。
///
/// **`debugfs` に読ませる**——**自分で書いて自分で読むと、同じ設計の取り違えが
/// 両側で相殺する。** **`i_size` までを出す**ので、**長さの一致がそのまま
/// `i_size` の正しさを主張する**（実測で確かめた）。
///
/// # 引けなかったことを、空と区別する
///
/// **`debugfs` は引けなくても終了コード 0 を返す**（実測）。**標準出力は空で、
/// 標準エラーへ `File not found by ext2_lookup` と出る。**
///
/// **区別していなかった。** そのため **S12-d の「空になったこと」は、
/// ファイルが消えていても満たされていた**——**あの判定は
/// 「0 バイトに縮んだ」を主張しているつもりで、
/// 「0 バイトに縮んだか、または存在しない」しか主張していなかった。**
///
/// **判定の前に濾す仕組みは、それ自体が判定の一部である**（S12-c で
/// `Fix? no` を `contains` で落として不満ごと消していたのと同じ形である）。
///
/// **S12-e の判定 C は、この区別の上に載っている**——
/// **消えたことを主張するので、「無い」が返ることそのものが判定である。**
fn debugfs_read(image: &Path, path: &str) -> Result<Option<Vec<u8>>> {
    let output = external_tool("debugfs")
        .env("LC_ALL", "C")
        .arg("-R")
        .arg(format!("cat {path}"))
        .arg(image)
        .output()
        .context(
            "failed to invoke debugfs (it ships with e2fsprogs, the same package as e2fsck and \
             mke2fs, which the kernel build script already requires)",
        )?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("File not found by ext2_lookup") {
        return Ok(None);
    }
    Ok(Some(output.stdout))
}

/// 割り当てたままにする構成の feature 名（S12-b）。
///
/// **破壊ではなく変種である**（`paging-test` と同じ形。壊さず、別の状態を作る）。
const KEEP_ALLOCATED_FEATURE: &str = "fs-alloc-keep-test";

/// `fs-image-copy` の行から複製先の物理範囲を読む。
fn parse_copy_range(line: &str) -> Option<(u64, u64)> {
    let rest = line.split("to phys ").nth(1)?;
    let range = rest.split_whitespace().next()?;
    let (start, end) = range.split_once("..")?;
    Some((parse_hex(start)?, parse_hex(end)?))
}

/// 空き数のひとそろい（S12-b）。**superblock と群 0 の分である。**
#[derive(Debug, PartialEq, Eq)]
struct FreeCounts {
    superblock_blocks: u64,
    superblock_inodes: u64,
    group_blocks: u64,
    group_inodes: u64,
    group_dirs: u64,
}

/// `dumpe2fs` に像の空き数を訊く（S12-b）。
///
/// **`e2fsck` と同じ `e2fsprogs` にある**ので、要る道具は増えない（実測で確かめた）。
/// `LC_ALL=C` は出力を言語設定に依らせないため（`run_e2fsck` と同じ理由）。
fn dumpe2fs_free_counts(image: &Path) -> Result<FreeCounts> {
    let output = external_tool("dumpe2fs")
        .env("LC_ALL", "C")
        .arg(image)
        .output()
        .context(
            "failed to invoke dumpe2fs (it ships with e2fsprogs, the same package as e2fsck and \
             mke2fs, which the kernel build script already requires)",
        )?;
    if !output.status.success() {
        bail!("dumpe2fs failed on {}: {}", image.display(), output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);

    // superblock 側は `Free blocks:` / `Free inodes:` の見出し行にある。
    // **群の一覧にも同じ語が出る**（`  Free blocks: 79-511` は範囲であって数ではない）
    // ので、**行頭で始まるものだけを取る。**
    let header = |key: &str| -> Option<u64> {
        text.lines()
            .find(|line| line.starts_with(key))
            .and_then(|line| line.split_once(':'))
            .and_then(|(_, value)| value.trim().parse().ok())
    };
    // 群の側は `  433 free blocks, 233 free inodes, 5 directories` の 1 行にある。
    let group = text
        .lines()
        .find(|line| line.contains(" free blocks, ") && line.contains(" free inodes, "))
        .map(|line| {
            let numbers: Vec<u64> = line
                .split_whitespace()
                .filter_map(|word| word.parse().ok())
                .collect();
            numbers
        })
        .unwrap_or_default();

    match (
        header("Free blocks:"),
        header("Free inodes:"),
        group.first(),
        group.get(1),
        group.get(2),
    ) {
        (Some(sb), Some(si), Some(gb), Some(gi), Some(gd)) => Ok(FreeCounts {
            superblock_blocks: sb,
            superblock_inodes: si,
            group_blocks: *gb,
            group_inodes: *gi,
            group_dirs: *gd,
        }),
        _ => bail!(
            "could not parse the free counts out of dumpe2fs for {}",
            image.display()
        ),
    }
}

/// 起動ログから、カーネルが読んだ空き数を取る。
fn parse_kernel_free_counts(serial: &str) -> Option<FreeCounts> {
    let sb = serial
        .lines()
        .find(|l| l.contains("ext2: superblock free counts: "))?;
    let group = serial
        .lines()
        .find(|l| l.contains("ext2: group 0 free counts: "))?;
    let field = |line: &str, key: &str| -> Option<u64> {
        line.split(key)
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    Some(FreeCounts {
        superblock_blocks: field(sb, "blocks=")?,
        superblock_inodes: field(sb, "inodes=")?,
        group_blocks: field(group, "blocks=")?,
        group_inodes: field(group, "inodes=")?,
        group_dirs: field(group, "dirs=")?,
    })
}

/// `fs-image-source` の行から、読んでいる先の物理アドレスを読む。
fn parse_reader_phys(line: &str) -> Option<u64> {
    let rest = line.split("(phys ").nth(1)?;
    parse_hex(rest.split(')').next()?)
}

/// 同じ行からカーネル像の物理範囲を読む。
fn parse_kernel_image_range(line: &str) -> Option<(u64, u64)> {
    let rest = line.split("the kernel image is ").nth(1)?;
    let range = rest.split_whitespace().next()?;
    let (start, end) = range.split_once("..")?;
    Some((parse_hex(start)?, parse_hex(end)?))
}

fn parse_hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

/// `--shell-test` を、既定ビルドで走らせるか破壊ビルドで走らせるか。
///
/// # 破壊の側は「落ちること」を期待するのではなく、裏返した主張を立てる
///
/// **他の破壊項目は `expected_markers` / `forbidden_markers` の対で書いてある**
/// （`CriticalTest`）。**あちらは「出る側」と「出ない側」を並べる形である。**
/// **`--shell-test` は判定がすべて真なら PASS という形なので、
/// そのままでは「落ちることを期待する項目」が書けない。**
///
/// **書けないのは判定の形ではなく、期待を固定していたことのほうだった。**
/// **期待を引数にすれば、破壊の側も「すべて真なら PASS」のままでよい。**
/// 矢印の判定だけが裏返り、残りは既定ビルドと同じく真であることを求める——
/// **これは「壊れるのは 1 つだけである」という主張になる。**
/// `no-eoi-test` が `forbidden_markers` で行っているのと同じ向きである。
///
/// **シリアルに出る目印がそのまま反転する。** 既定は `zash: pyq: cannot run`、
/// 破壊は `zash: pqy: cannot run` である。**他の破壊項目とまったく同じ
/// 「シリアルに含まれる / 含まれない」の形なので、判定の作り替えは要らない。**
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShellTestMode {
    /// 既定ビルド。左矢印が挿入点を動かす。
    Normal,
    /// `KEYMAP=us` を持ち越した回（f-1b）。**像を作り直さない。**
    ///
    /// **1 度目の起動が `zi` で `/etc/environment` へ `KEYMAP=us` を足し、
    /// この回はその像の上で起きる。** **判定は 1 本だけ裏返る**
    /// ——**打つ物理キーは同じで、出る字が変わる。**
    KeymapUs,
    /// 破壊（`keymap-always-jis-test`。f-1b）。**引く側が選択を見ない。**
    ///
    /// **カーネルの `keymap:` の行は `us` のままで、出る字だけが JIS である。**
    KeymapUsAlwaysJis,
    /// 破壊（`keyboard-drop-arrows-test`）。矢印がデコーダで未対応へ戻るので、
    /// 前景へ 3 バイトが届かず、挿入点が動かない。
    ArrowsDropped,
    /// 破壊（`keyboard-drop-esc-test`。zi-a）。Esc がデコーダで未対応へ戻るので、
    /// 前景へ `\x1b` が届かず、**実打鍵の Esc `[` `D` が CSI にならない**——
    /// 3 打が字のまま行へ入る。[`ShellTestMode::ArrowsDropped`] と同じく
    /// 判定を 1 本裏返す形である。
    EscDropped,
    /// 破壊。**通らないことを期待する**（S12 前の手当て、C）。
    ///
    /// # なぜこちらは裏返さないのか
    ///
    /// **[`ShellTestMode::ArrowsDropped`] は判定を 1 本裏返せば済んだ。**
    /// **壊れるのが 1 本だと分かっていたからである。**
    ///
    /// **中断の破壊は 5 つあり、落ちる判定が 1 本ずつ違う**
    /// （子が止まらない / シェルが余分に起こし直される / `^C` が 2 つ出る）。
    /// **5 通りの裏返しを書くと、破壊ごとに期待を書き写すことになり、
    /// 「どれか 1 本が落ちる」を 5 回別々に述べる形になる。**
    ///
    /// **主張しているのは「この破壊は `--shell-test` が捕まえる」である。**
    /// **それは「通らないこと」そのものなので、そう書く。**
    /// **どの判定が落ちたかは出力に並ぶ**ので、読めば分かる。
    ///
    /// **起動しなかった場合は Ok にしない。** あちらは環境の失敗で、
    /// **捕まえたことにはならない**（`classify_boot` が先に切り分ける）。
    MustFail(&'static str),
}

/// `--shell-test` が「通らないこと」で捕まえる破壊（S12 前の手当て、C）。
///
/// **実測で落ちることを確かめてある。** 落ちる判定はそれぞれ違う。
///
/// # 中断だけの一覧ではなくなった（DIR-1）
///
/// **`env-drop-path-test` が加わった。** **`PATH` が届かないと、名前だけで
/// 打った語が起こせない**——**落ちるのは「bare names resolved under /bin」
/// だけである**（`/bin/ls` のようにパスを直に打つ形は動く）。
///
/// **名前を `KILL_SABOTAGES` のままにしない。** **一覧の名前が中身と
/// 食い違うと、次に足す者が「中断ではないから別の一覧が要る」と考える。**
/// `utf8-test` を「通らないこと」で回す破壊（`ADR-0054`）。
///
/// **落ちる判定が違う**——**幅の側は 2 本（全角のセル数と画面の桁）、
/// 丸ごと落とす側は 1 本（壊れたバイトが行を消さない）である。**
const UTF8_TEST_SABOTAGES: &[&str] = &[
    "width-always-one-test",
    "console-drop-invalid-chunk-test",
    // **`a` がバイトで進む形（2026-09-03）。** **多バイトの段で見落としていた**
    // ——**台本が全角の上で `a` を打っていなかったので、判定も捕まえていなかった。**
    "zi-append-by-byte-test",
    // **VIM-1 で 4 つ増えた。** **`$` / `^` / `A` / `o` を足したので、台本が
    // `/data/vimops` を開くようになった。**
    //
    // **`zi-line-end-stays-test` は 3 本落ちる**——**`$` の判定と、同じ道を
    // 通る `A` と `o` の判定である**（**`A` と `o` に破壊を置かない判断が、
    // 同時に「両方が `$` に寄っている」ことの主張になる**。運用者の指示。
    // 2026-09-01）。
    // **`zi-enter-does-nothing-test` は `o` を落とす**——**`o` は `A` の後に
    // インサートの Enter を打つのと同じ道である。**
    "zi-line-end-stays-test",
    "zi-first-nonblank-to-zero-test",
    "zi-escape-by-byte-test",
    "zi-enter-does-nothing-test",
    // **VIM-1b で 1 つ増えた。** **状態行の `行:桁` が古くなる形である。**
    "zi-status-stale-column-test",
];

/// `profile-test` を「通らないこと」で回す破壊（PR-1）。
///
/// **3 つとも、落ちる判定が 1 本ずつ違う。**
///
/// **`shell-skip-profile`（設定を読まない）は置いていない**——**落ちる判定が
/// `shell-profile-first-line-only` と重なり、その形でしか落ちない判定を
/// 持たないためである**（運用者の判断。2026-09-04）。
/// `history-test` を「通らないこと」で回す破壊（HI-1）。
///
/// **2 つとも、落ちる判定が 1 本ずつ違う。**
///
/// **`shell-history-order-reversed`（新しいものから書く）は置いていない**
/// ——**書かない破壊が落とす判定の部分集合になる**（**書かなければ順序の
/// 判定も落ちる**）。**その形でしか落ちない判定を持たない。**
const HISTORY_TEST_SABOTAGES: &[&str] = &[
    "shell-history-not-saved-test",
    "shell-history-missing-is-error-test",
];

const PROFILE_TEST_SABOTAGES: &[&str] = &[
    "shell-profile-order-swapped-test",
    "shell-profile-first-line-only-test",
    "shell-profile-missing-is-error-test",
];

/// `complete-test` を「通らないこと」で回す破壊（TAB-1）。
///
/// **4 つとも、落ちる判定が 1 本ずつ違う。**
///
/// **「Tab を捨てる」は置いていない**——**落ちる判定が他の部分集合になる**
/// （`skip-profile` と `history-order-reversed` と同じ判断である）。
const COMPLETE_TEST_SABOTAGES: &[&str] = &[
    "shell-complete-no-common-prefix-test",
    "shell-complete-keeps-duplicates-test",
    "shell-complete-ignores-path-test",
    "shell-complete-silent-when-no-progress-test",
];

/// `fp-test` を「通らないこと」で回す破壊（B-a。`ADR-0058`）。
///
/// **3 つで、落ちる判定が 1 本ずつ違う。**
///
/// **「切り替えで復元しない」は置いていない**——**落とす判定が作れなかった。**
/// **Ring 3 が走っている間、走行可能なタスクはメインだけで、切り替えそのものが
/// 起きない**（`ADR-0058` の「決定 1 の判定」）。**もう 1 人の使い手を用意しよう
/// として、切り替えが遠征の RSP0 と噛み合わないことが分かった**（実測。
/// `docs/troubleshooting.md` の 2026-09-07）。
const FP_TEST_SABOTAGES: &[&str] = &[
    "fp-no-fresh-state",
    "fp-spawn-no-save",
    "fp-mf-not-foldable-test",
];

const SHELL_TEST_SABOTAGES: &[&str] = &[
    "kill-ignore-interrupt-test",
    "kill-fold-at-depth-one-test",
    "kill-keep-stale-interrupt-test",
    "kill-fold-keep-bkl-test",
    "kill-keep-typed-input-test",
    "env-drop-path-test",
    // **SE-a と SE-b で 3 つ増えた（`ADR-0050`）。** 落ちる判定はそれぞれ違う——
    // **Home / End の 1 本、Ctrl+A / Ctrl+E の 1 本、制御バイトの 1 本である。**
    //
    // **実測で、落ちた判定はこうだった**（2026-08-28。`--full`）。
    // **`keyboard-drop-home-end-test` は 1 本**（Home と End）。
    // **`keyboard-drop-ctrl-letters-test` は 2 本**（Ctrl+A / Ctrl+E と Ctrl+D）
    // ——**Ctrl+英字そのものを外すので、Ctrl+英字を使う判定は全部落ちる。**
    // **`shell-keep-control-bytes-test` は 2 本**（Tab と Ctrl+D）。
    //
    // **`keyboard-drop-ctrl-letters-test` は、止めた子の判定を落とさない**
    // （実測で緑のままだった）。**中断の旗は割り込み側の別経路だからである**
    // （`ADR-0050` の条件 1）。
    // **どの判定が落ちたかは出力に並ぶ**（[`ShellTestMode::MustFail`] の doc）。
    "keyboard-drop-home-end-test",
    "keyboard-drop-ctrl-letters-test",
    "shell-keep-control-bytes-test",
    // **SE-d で 2 つ増えた（`ADR-0049`）。**
    // **`shell-skip-expansion-test` は 4 本とも落とす**（どの行も展開に寄りかかる）。
    //
    // **`shell-keep-empty-word-test` は置かなかった。** **書いて走らせたが
    // 捕まらなかった**（実測。2026-08-28）——**空の語を落とさなくても、
    // `zash` の語へ切る側が空白の連なりを読み飛ばすので `argv` が変わらない。**
    // **「状態が変わらない」で緑になる形である**（`ADR-0049` の Addendum）。
    "shell-skip-expansion-test",
    // **SE-c で 1 つ増えた。** **履歴を積まない。**
    // **「上で辿れた」の判定は、既定の構成ではこの破壊でしか落ちない**
    // ——**矢印を落とす破壊は期待のほうを裏返すので、落ちない。**
    "shell-drop-history-test",
    // **SE-f で 1 つ増えた。** **消す範囲の先頭を 1 つずらす。**
    // **`keyboard-drop-ctrl-letters-test` が覆うのは「鍵が届くこと」であって、
    // 「範囲の計算が正しいこと」ではない**——**`Ctrl+K` が行頭まで消す形は、
    // 鍵が届いているのであちらでは捕まらない。**
    "shell-shift-delete-range-test",
    // **f-2 で 1 つ増えた（`ADR-0053`）。** **`export` した表を子へ積まない。**
    // **落ちるのは「子が見た `envc`」の 1 本だけである**——**シェルの表は
    // 引けるままなので、`echo $ZF2` も `set` も緑である。** **その形でしか
    // 落ちない判定が在るので置いた。**
    "shell-export-not-pushed-test",
];

impl ShellTestMode {
    /// この形で立てる feature。
    fn features(self) -> &'static [&'static str] {
        match self {
            ShellTestMode::Normal | ShellTestMode::KeymapUs => &[],
            ShellTestMode::KeymapUsAlwaysJis => &["keymap-always-jis-test"],
            ShellTestMode::ArrowsDropped => &["keyboard-drop-arrows-test"],
            ShellTestMode::EscDropped => &["keyboard-drop-esc-test"],
            // **1 要素の配列を作れないので、一覧から借りる。**
            // `SHELL_TEST_SABOTAGES` に在る名前だけを受け取る契約である。
            ShellTestMode::MustFail(feature) => {
                let index = SHELL_TEST_SABOTAGES
                    .iter()
                    .position(|name| *name == feature)
                    .expect("MustFail takes a feature listed in SHELL_TEST_SABOTAGES");
                &SHELL_TEST_SABOTAGES[index..index + 1]
            }
        }
    }

    /// 判定行の頭。**破壊の側を別の名前にする**——`--full` の出力で
    /// どちらの実行かが読めないと、落ちた行の出所が分からない。
    fn context(self) -> String {
        match self {
            ShellTestMode::Normal => "shell-test".to_string(),
            ShellTestMode::KeymapUs => "shell-test keymap=us".to_string(),
            ShellTestMode::KeymapUsAlwaysJis => "shell-test keymap-always-jis".to_string(),
            ShellTestMode::ArrowsDropped => "shell-test keyboard-drop-arrows".to_string(),
            ShellTestMode::EscDropped => "shell-test keyboard-drop-esc".to_string(),
            ShellTestMode::MustFail(feature) => format!("shell-test {feature}"),
        }
    }

    /// シリアルの記録先。**互いに上書きしない**——落ちたときに
    /// 両方のログが残っていないと、どちらが壊れたのかを後から見られない。
    fn serial_log_name(self) -> String {
        match self {
            ShellTestMode::Normal => "shell-test-serial.log".to_string(),
            ShellTestMode::KeymapUs => "shell-test-keymap-us-serial.log".to_string(),
            ShellTestMode::KeymapUsAlwaysJis => {
                "shell-test-keymap-always-jis-serial.log".to_string()
            }
            ShellTestMode::ArrowsDropped => "shell-test-drop-arrows-serial.log".to_string(),
            ShellTestMode::EscDropped => "shell-test-drop-esc-serial.log".to_string(),
            ShellTestMode::MustFail(feature) => format!("shell-test-{feature}-serial.log"),
        }
    }

    /// 矢印について期待すること。**`true` は「挿入点が動く」である。**
    ///
    /// **破壊の側も真である。** 中断の破壊はどれも矢印に触らない。
    fn expects_the_cursor_to_move(self) -> bool {
        self != ShellTestMode::ArrowsDropped
    }

    /// 変換表について期待すること（f-1b）。**`true` は「US の表を引く」。**
    fn expects_the_us_layout(self) -> bool {
        // **破壊の側も真である。** **期待は「US の字が出ること」のままで、
        // 引く側が見ないので落ちる**——**期待を裏返すと、破壊が緑になる。**
        matches!(
            self,
            ShellTestMode::KeymapUs | ShellTestMode::KeymapUsAlwaysJis
        )
    }

    /// この回は `disk0.img` を作り直さないか（f-1b）。
    ///
    /// **`KEYMAP=us` は 1 度目の起動が `zi` で書き込んだものなので、
    /// 作り直すと消える。**
    fn keeps_the_disk(self) -> bool {
        matches!(
            self,
            ShellTestMode::KeymapUs | ShellTestMode::KeymapUsAlwaysJis
        )
    }

    /// Esc の実打鍵について期待すること（zi-a）。**`true` は「Esc `[` `D` の
    /// 3 打が CSI として解釈され、挿入点が動く」である。**
    fn expects_esc_to_reach_ring3(self) -> bool {
        // **US の回も偽である（f-1b）。** **Esc は届いているが、
        // 台本が `[` を作るのに打っている `bracket_right`（`0x1B`）は、
        // US では `]` である**——**CSI にならないので挿入点が動かない。**
        // **これは US の表が効いていることの、もう 1 つの現れである。**
        !matches!(
            self,
            ShellTestMode::EscDropped | ShellTestMode::KeymapUs | ShellTestMode::KeymapUsAlwaysJis
        )
    }

    /// 表の外の 2 キーについて期待すること（f-1b）。
    ///
    /// **US では何も出ない。** **運用者が目視で見る項目の 1 つでもある。**
    fn expects_the_jis_only_keys(self) -> bool {
        !self.expects_the_us_layout()
    }

    /// `~` の展開について期待すること（f-1b）。
    ///
    /// **US では `~` そのものが打てない**——**台本は `shift-equal`
    /// （`0x0D` の Shift）で `~` を作っており、US ではあれが `+` である。**
    fn expects_the_tilde_to_expand(self) -> bool {
        !self.expects_the_us_layout()
    }

    /// `export` の台本が効くことを期待するか（f-2 の後に足した）。
    ///
    /// **US では `=` が打てない**——**台本は `shift-minus`（JIS の `-` の Shift）で
    /// `=` を作っており、US ではあれが `_` である。** **`export ZF2_exported` に
    /// なるので、シェルは「名前ではない」と断る。**
    ///
    /// **断る側も主張である**——**`~` の判定と同じ形で、両側を見る。**
    fn expects_the_export_script(self) -> bool {
        !self.expects_the_us_layout()
    }

    /// この形が通ることを期待するか。**破壊は通らないことを期待する。**
    fn expects_to_pass(self) -> bool {
        !matches!(self, ShellTestMode::MustFail(_))
    }
}

/// シェルへ打鍵を送り、組み込みの `exit` が効くことを見る（S11-11）。
///
/// # 既定の起動ログには入れない
///
/// **`sendkey` はタイミングに依存する。** `cmd_keyboard_test` は同じ性質で
/// **既に `flaky` へ移してある**（確率的な 4 項目の 1 つ）。
/// **同じものを既定の起動ログへ入れると、参照が揺れる。**
///
/// # 何を見るか
///
/// **この刻みのシェルは組み込みの `exit` しか持たない。** したがって
/// **「`exit` と改行を送ったらシェルが終わり、`init` が起こし直す」**が
/// **唯一の観測である。** **`init` の役目も同時に確かめられる。**
///
/// # 止まった場所が分かる形で送る
///
/// **送る前に、シェルがプロンプトを出すまで待つ。** 出ていなければ
/// **打鍵の前で止まっている。**
/// **送った後は `init: the shell ended` の行を見る**——あの行が
/// **リングが受けたスキャンコード数と、前景が Ring 3 へ渡したバイト数**を
/// 出しているので、**どこで止まったかが 1 行で分かる。**
///
/// # 破壊も同じ関数で走らせる
///
/// **[`ShellTestMode`] を見ること。** 打鍵を流す仕組みは 1 つで足りる。
/// PCI 列挙の破壊の一覧（S13-a）。
const PCI_SABOTAGES: &[(&str, &str)] = &[
    ("a shifted ID register", "pci-config-offset-test"),
    (
        "an ignored multifunction bit",
        "pci-ignore-multifunction-test",
    ),
    (
        "a scan that stops at the first device",
        "pci-stop-at-first-test",
    ),
];

/// カーネルの PCI 列挙を、QEMU 自身の帳簿（`info pci`）と突き合わせる（S13-a）。
///
/// **期待値を定数で持たない。** bus / device の並びは QEMU の側の事情なので、
/// 同じ起動の QEMU から `info pci` で取り、カーネルの判定行と両側から比べる
/// （S12 の `dumpe2fs` と同じ形——外の道具が独立した答えを持っている）。
/// ANSI の解釈の実演（zi-b）。起動シーケンスの判定行を読む。
///
/// **`sendkey` を使わないので決定的である**——`ansi-test` feature の
/// exercise が起動中に CSI を前景経路へ流し、カーソル位置とセルの中身を
/// 判定行に出す。ここはそれを読むだけである。
///
/// `--sabotage` に破壊 feature（`ansi-console-skip-parse-test`）を与えると
/// 一緒に立てる。**その場合の期待（落ちること）は呼び出し側が見る**
/// （`cmd_fs_image_extract` と同じ形）。
fn cmd_ansi_test(features: &[&str]) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let mut all_features: Vec<&str> = vec!["ansi-test"];
    all_features.extend_from_slice(features);
    let kernel_elf = build_kernel_with_features(&workspace_root, &all_features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    // **構成ごとに別のログへ書く**（`cmd_fs_image_extract` と同じ理由）。
    let tag = all_features.join("-");
    let serial_log = workspace_root
        .join("target")
        .join(format!("ansi-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the ansi test")?;

    // 実演の終端行を待つ。上限つき。
    let done_marker = "ansi-test: done";
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if text.contains(done_marker) {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let context = if features.is_empty() {
        "ansi-test".to_string()
    } else {
        format!("ansi-test {}", features.join("+"))
    };
    let context = context.as_str();

    // **落ちる前に、起動の失敗と切り分ける**（他の QEMU 項目と同じ作法）。
    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    // **判定は exercise が出した 6 行である。** それぞれ「出たか」を見る——
    // 破壊ビルドでは値が false になるか、カーソルがずれて期待の行が出ない。
    let judgements: &[(&str, &str)] = &[
        (
            "CUP moved the cursor",
            "cursor after CUP(3;7) = (6, 2) (expected (6, 2)), matches = true",
        ),
        (
            "printing at the cursor left ink",
            "the cell under CUP got ink = true, cursor advanced = true",
        ),
        (
            "a split sequence still parsed",
            "a sequence split across two writes still moved the cursor = true",
        ),
        (
            "EL(2) erased the line",
            "EL(2) erased the line and left the cursor = true",
        ),
        (
            "EL(0) erased right and kept left",
            "EL(0) erased right of the cursor and kept the left = true",
        ),
        (
            "EL(1) erased left",
            "EL(1) erased left of the cursor = true",
        ),
        (
            "ED(0) erased below and kept above",
            "ED(0) erased below and kept above = true",
        ),
        (
            "ED(1) erased above",
            "ED(1) erased above and up to the cursor = true",
        ),
        (
            "ED(2) erased the display",
            "ED(2) erased the display and left the cursor = true",
        ),
        (
            "SGR was consumed silently",
            "an SGR sequence was consumed without printing = true",
        ),
        (
            "SGR colored the cell and the screen",
            "SGR reached the cell = true, and the screen really shows that background = true",
        ),
        (
            "SGR 0 restored the defaults",
            "SGR 0 restored the default colors = true",
        ),
        (
            "the cursor is drawn where CUP put it",
            "the cursor is drawn where CUP put it = true",
        ),
        ("DECTCEM hid the cursor", "DECTCEM hid the cursor = true"),
        (
            "DECTCEM brought the cursor back",
            "DECTCEM brought the cursor back = true",
        ),
        (
            "moving the cursor left no trail",
            "moving the cursor left no trail = true",
        ),
    ];
    let mut all_ok = true;
    for (name, needle) in judgements {
        let ok = serial.contains(needle);
        println!("{context}: {name} = {ok}");
        all_ok &= ok;
    }
    if !serial.contains(done_marker) {
        println!("{context}: the exercise did not reach its end marker");
        all_ok = false;
    }

    if all_ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAILED")
    }
}

/// 指定の feature で起動し、シリアルに目印が出ることを見る（ADR-0038）。
///
/// **判定行そのものを見る形である。** 破壊の側では目印が出ないので `Err` になる。
/// **起動しなかった場合も `Err` だが、`classify_boot` が先に切り分ける。**
fn cmd_boot_with_features(features: &[&str], marker: &str, wanted: &str) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = features.join("-");
    let serial_log = workspace_root
        .join("target")
        .join(format!("boot-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64")?;

    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if text.lines().any(|line| line.contains(marker)) {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let context = format!("boot {}", features.join("+"));
    let held = serial
        .lines()
        .any(|line| line.contains(marker) && line.contains(wanted));
    println!("{context}: {marker} says {wanted} = {held}");
    if held {
        Ok(())
    } else {
        bail!("{context}: {marker} did not say {wanted}")
    }
}

/// `zi` の実演（zi-d）。**決定的な台本入力で駆動する。**
///
/// **`sendkey` を使わない。** 台本は `read_bytes` が返すので、打鍵の間隔にも
/// 行の処理の速さにも依らない（`kernel/src/input.rs` の `script`）。
/// **`--shell-test` は実打鍵の側を主張しており、層が違う**——あちらが
/// 42.58 秒（実測）掛かるのは待ちのためである。
///
/// **判定は `zi` の内部状態である**（`kernel/userland/zi.rs` のモジュール doc）。
/// 画面に正しく描けたことは、この項目では観測できない。
/// ログを、UTF-8 でないバイトを落として読む（`ADR-0054` の後の手当て）。
///
/// # 「失敗を空に落とす」形を 1 箇所へ寄せる
///
/// **`fs::read_to_string(...).unwrap_or_default()` は、読めなかったときに
/// 空文字を返す**——**判定は「何も出ていない」として落ちる。** **落ちた理由が
/// 「プログラムが壊れた」に見えるが、実際は「ログが UTF-8 として読めなかった」
/// である。**
///
/// **実測で踏んだ**（2026-09-01）。**`utf8-test` の台本が `/data/badutf8` を
/// 画面へ出し、その中身がシリアルへも出た。** **`xtask` は「カーネルが起動
/// しなかった」と報告した**——**2 回続けて同じ形だったので、環境の揺れでは
/// ないと分かった。**
///
/// **同じ形は 2 度目である**（1 度目は `find` の失敗を空として扱い、
/// 「差が 0 ブロック」というもっともらしい結果を得た。`docs/troubleshooting.md`）。
/// **したがって 1 箇所へ寄せ、ログを読む全箇所をここへ通した。**
fn read_lossy(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

/// 多バイトの字の判定（`ADR-0054`）。**1 回の起動で 4 つ見る。**
///
/// # なぜ `zi` で見るのか
///
/// **`zi` は本文を画面の先頭行から描く。** **`cat` の出力は画面の下へ流れるので、
/// 先頭 6 行を読む観測点（`OBSERVE_TEXT_ROWS`）に載らない。**
///
/// # 判定は 9 つである
///
/// 1. **壊れたバイトが行を消さないこと**（`/data/badutf8` が `x?y` と出る）
/// 2. **全角が 2 セルを占めること**（`/data/utf8` が `? ? u` と出る。**間の空白は
///    右半分のセルである**）
/// 3. **画面の桁が字で進むこと**（`l` の後に `col=3 scol=2` の `move`）
/// 4. **消すのが字であること**（保存後の装置の中身が `あu` である。**バイトを
///    割っていない**）
///
/// **VIM-1 で 5 本足した**（`/data/vimops` は `  あいu` である）。
///
/// 5. **`$` が行末の字へ動くこと**（`col=8 scol=6` の `line-end`。**バイト長の
///    9 ではない**）
/// 6. **`^` が最初の非空白へ動くこと**（`col=2 scol=2` の `first-nonblank`）
/// 7. **Esc が字の境界へ戻ること**（`col=2 scol=2` の `normal`。**全角の上で
///    `a` を打った直後なので、バイトで戻すと字の途中へ落ちる**）
/// 8. **1 行目が `  いuX` であること**（**`x` が `あ` を丸ごと消し、`A` が
///    行末から `X` を挿した**）
/// 9. **`o` が下に行を開いたこと**（**2 行目が `Y` である**）
///
/// **VIM-1b で 1 本足した。**
///
/// 10. **状態行がカーソルに追いつくこと**（`$` の直後に `1:9` と出ている。
///     **画面の実物を読む**——`\x02`）
///
/// # 3 の札を見るのは、判定が別の行に当たらないようにするためである
///
/// **`col=3 scol=2` は、`l` の後の `move` にも、`Z` を入れた後の Esc の
/// `normal` にも出る。** **札を見ないと、`l` が動かなくなっても Esc の行で
/// 緑になりうる**（判定の当たり先がずれる形）。
fn cmd_utf8_test(features: &[&str], expect_pass: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let mut all_features: Vec<&str> = vec!["utf8-test"];
    all_features.extend_from_slice(features);
    let kernel_elf = build_kernel_with_features(&workspace_root, &all_features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = all_features.join("-");
    let serial_log = workspace_root
        .join("target")
        .join(format!("utf8-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the utf8 test")?;

    // **合図は `script-done:` である**（`cmd_zi_test` と同じ。何も主張しない観測点）。
    //
    // # ここだけは UTF-8 として読めない
    //
    // **台本が `/data/badutf8` を開くので、その中身がそのままシリアルへ出る**
    // ——**`fs::read_to_string` は失敗し、`unwrap_or_default()` が空文字を返す。**
    // **実測で踏んだ**（2026-09-01。**「カーネルが起動しなかった」と報告された**）。
    // **バイトで読んで、落として直す。**
    let started_waiting = Instant::now();
    let deadline = started_waiting + ZI_TEST_TIMEOUT;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if strip_ansi(&text).contains("script-done:") {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let context = if features.is_empty() {
        "utf8-test".to_string()
    } else {
        format!("utf8-test {}", features.join("+"))
    };
    let context = context.as_str();

    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    // **先頭行の観測を打った順に拾う。** 1 本目が `badutf8`、2 本目が `utf8` である。
    let rows: Vec<String> = strip_ansi(&serial)
        .lines()
        .filter(|line| line.contains("screen-text: row 0 says "))
        .filter_map(|line| line.split("says ").nth(1).map(|rest| rest.to_string()))
        .collect();
    let broken_row = rows.first().cloned().unwrap_or_default();
    let wide_row = rows.get(1).cloned().unwrap_or_default();

    // **判定 1**——**壊れたバイトが行を消さない。**
    // **`0xFF` は置換文字になり、非 ASCII なので観測点は `?` と書く。**
    let broken_line_survived = broken_row.contains("x?y");
    // **判定 2**——**全角は 2 セルである。** **間の空白が右半分のセルである。**
    let wide_takes_two_cells = wide_row.contains("? ? u");
    // **判定 3**——**画面の桁が字で進む。** **`col` はバイト、`scol` は幅の合計である。**
    let stripped = strip_ansi(&serial);
    let cursor_says = |column: &str, tag: &str| {
        stripped.lines().any(|line| {
            line.contains("zi: cursor") && line.contains(column) && line.trim_end().ends_with(tag)
        })
    };
    let column_counts_characters = cursor_says("col=3 scol=2", "move");
    // **判定 4**——**消すのは字である。** **`あいu` から `い` を消して `あu` になる。**
    let disk = disk_image_path(&esp_dir);
    let saved = debugfs_read(&disk, "/data/utf8")?;
    // **`x` で `い` が消え、`a` で全角の次へ動いてから `Z` を入れた。**
    // **`a` がバイトで進むと、`Z` が `あ` の途中へ入って中身が壊れる。**
    let deleted_a_character = saved.as_deref() == Some("あZu\n".as_bytes());

    // **判定 5**——**`$` は行末の「字」へ動く。** **`  あいu` は 9 バイトで、
    // 最後の字 `u` の先頭は 8 バイト目、桁は 6 である。**
    let line_end_is_a_character = cursor_says("row=0 col=8 scol=6", "line-end");
    // **判定 6**——**`^` は最初の非空白へ動く。** **空白 2 つを飛ばした先は
    // `あ` の先頭で、バイトも桁も 2 である。**
    let first_nonblank_skips_the_blanks = cursor_says("row=0 col=2 scol=2", "first-nonblank");
    // **判定 7**——**Esc は字の境界へ戻る。** **全角の上で `a` を打った直後
    // なので、バイトで戻すと `あ` の途中（4 バイト目）へ落ちる。**
    let escape_lands_on_a_boundary = cursor_says("row=0 col=2 scol=2", "normal");
    // **判定 10**——**状態行がカーソルに追いつく（VIM-1b）。**
    // **`$` の直後に画面から読む**（`\x02`）。**`  あいu` の行末は 9 桁目で、
    // 状態行は 1 起点で出す**ので `1:9` である。
    // **診断の行ではなく、画面の実物を読んでいる**——**状態行は人が見る
    // ためだけに在るので、出している数そのものを見る必要がある。**
    let status_row_says = stripped
        .lines()
        .filter_map(|line| line.split("screen-status: ").nth(1))
        .next_back()
        .unwrap_or_default()
        .to_string();
    let status_follows_the_cursor = status_row_says.contains("1:9");
    // **判定 8 と 9**——**装置の中身で見る。**
    // **`x` が `あ` を丸ごと消し、`A` が行末から `X` を挿し、`o` が下に
    // 行を開いて `Y` を載せた形である。**
    let saved_vimops = debugfs_read(&disk, "/data/vimops")?;
    let vimops = saved_vimops.as_deref().unwrap_or(&[]);
    let the_line_kept_its_characters = vimops.starts_with("  いuX\n".as_bytes());
    let opened_a_line_below = vimops.split(|byte| *byte == b'\n').nth(1) == Some(&b"Y"[..]);

    println!("{context}: the broken byte did not erase the line = {broken_line_survived} (row {broken_row})");
    println!(
        "{context}: a wide character takes two cells = {wide_takes_two_cells} (row {wide_row})"
    );
    println!("{context}: the screen column counts characters = {column_counts_characters}");
    println!(
        "{context}: deleting and appending stayed on character boundaries = \
         {deleted_a_character} (the device says {:?})",
        saved.as_deref().map(String::from_utf8_lossy)
    );
    println!("{context}: $ moved to the last character = {line_end_is_a_character}");
    println!("{context}: ^ skipped the leading blanks = {first_nonblank_skips_the_blanks}");
    println!("{context}: escape landed on a character boundary = {escape_lands_on_a_boundary}");
    println!(
        "{context}: the line kept its characters = {the_line_kept_its_characters} \
         (the device says {:?})",
        saved_vimops.as_deref().map(String::from_utf8_lossy)
    );
    println!("{context}: o opened a line below = {opened_a_line_below}");
    println!(
        "{context}: the status line caught up with the cursor = \
         {status_follows_the_cursor} ({status_row_says})"
    );

    let passed = broken_line_survived
        && wide_takes_two_cells
        && column_counts_characters
        && deleted_a_character
        && line_end_is_a_character
        && first_nonblank_skips_the_blanks
        && escape_lands_on_a_boundary
        && the_line_kept_its_characters
        && opened_a_line_below
        && status_follows_the_cursor;
    if passed {
        println!("{context}: PASS");
        if expect_pass {
            Ok(())
        } else {
            bail!("{context}: the sabotage was NOT caught; every judgement still held")
        }
    } else {
        println!("{context}: FAILED");
        if expect_pass {
            bail!("{context}: FAILED")
        } else {
            println!("{context}: the sabotage was caught (this run is expected to fail)");
            Ok(())
        }
    }
}

/// 起動時の設定の判定（PR-1）。**1 回の起動で 2 度シェルを起こす。**
///
/// # 判定は 3 つである
///
/// 1. **2 行目まで走り、`$NAME` が展開されたこと**
///    （`echo $ZPROFILE_SOURCE` が `from-etc-profile`）
/// 2. **後のほうが勝つこと**（`echo $ZPROFILE` が `root-profile`。
///    **`/etc/profile` が置いた値を `/root/.profile` が上書きした**）
/// 3. **無いときは何も言わないこと**（`/root/.profile` を消して起こし直すと、
///    `echo $ZPROFILE` が `etc-profile` に戻り、**消したパスを名指す行が
///    出ていない**）
///
/// # 値を長くしてある
///
/// **`etc` と `home` で始めたら、`etc` が起動ログの `ls /` の出力に
/// 当たった**（実測。2026-09-04。**起動シーケンスの `syscall-test` が
/// `ls` を起こしており、その 1 行がまるごと `etc` である**）。
/// **判定の当たり先がずれる族である。** **値を `etc-profile` /
/// `root-profile` / `from-etc-profile` にして、像の他の行と当たらない
/// 形にした。**
///
/// # 出力の数え方
///
/// **`echo` の出力は 1 行まるごとがその値である**（実測。プロンプトと
/// 打った語はその前の行に在り、`zash` は台本の経路で反響しない）。
/// **行がまるごと一致することを見る**——**部分一致にすると起動ログの
/// 他の行に当たりうる**（`etc` は短い）。
/// **1 度目と 2 度目は `init` の起こし直しの行で分ける。**
fn cmd_profile_test(features: &[&str], expect_pass: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let mut all_features: Vec<&str> = vec!["profile-test"];
    all_features.extend_from_slice(features);
    let kernel_elf = build_kernel_with_features(&workspace_root, &all_features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = all_features.join("-");
    let serial_log = workspace_root
        .join("target")
        .join(format!("profile-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the profile test")?;

    // **合図は `script-done:` である**（`cmd_utf8_test` と同じ）。
    let deadline = Instant::now() + ZI_TEST_TIMEOUT;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if strip_ansi(&text).contains("script-done:") {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let context = if features.is_empty() {
        "profile-test".to_string()
    } else {
        format!("profile-test {}", features.join("+"))
    };
    let context = context.as_str();

    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    let stripped = strip_ansi(&serial);
    // **1 度目と 2 度目を分ける。** **`init` の起こし直しの行が境である。**
    let restart_marker = "init: starting /bin/zash (restart 1 of 3)";
    let (first, second) = match stripped.find(restart_marker) {
        Some(at) => (&stripped[..at], &stripped[at..]),
        None => (stripped.as_str(), ""),
    };
    // **`echo` の出力はそれだけで 1 行になる。** **プロンプトと打った語は
    // その前の行に在る**（`zash` は台本の経路で反響しない）。
    // **行がまるごとその値であることを見る**——**部分一致にすると、
    // 起動ログの他の行に当たりうる。**
    let says = |text: &str, value: &str| text.lines().any(|line| line.trim() == value);

    // **判定 1**——**2 行目まで走り、`$ZPROFILE` がその場で展開された。**
    let every_line_ran = says(first, "from-etc-profile");
    // **判定 2**——**後のほうが勝つ。** **`/root/.profile` が `etc` を覆した。**
    let the_user_profile_wins = says(first, "root-profile");
    // **判定 3**——**無いときは何も言わない。** **2 度目は `/etc/profile`
    // しか無いので `etc` へ戻り、消したパスを名指す行は出ない。**
    let missing_is_silent = says(second, "etc-profile") && !second.contains("/root/.profile");

    println!("{context}: every line of /etc/profile ran = {every_line_ran}");
    println!("{context}: ~/.profile won over /etc/profile = {the_user_profile_wins}");
    println!("{context}: a missing profile said nothing = {missing_is_silent}");

    let passed = every_line_ran && the_user_profile_wins && missing_is_silent;
    if passed {
        println!("{context}: PASS");
        if expect_pass {
            Ok(())
        } else {
            bail!("{context}: the sabotage was NOT caught; every judgement still held")
        }
    } else {
        println!("{context}: FAILED");
        if expect_pass {
            bail!("{context}: FAILED")
        } else {
            println!("{context}: the sabotage was caught (this run is expected to fail)");
            Ok(())
        }
    }
}

/// 履歴がファイルで持ち越されることの判定（HI-1）。**2 度シェルを起こす。**
///
/// # 判定は 3 つである
///
/// 1. **前の起動で打った行が辿って戻ること**（2 度目に上 2 回で
///    `/bin/echo hist-two` が走り、`hist-two` が出る）
/// 2. **ファイルが古い順であること**（装置の `/root/.zash_history` で、
///    `hist-one` の行が `hist-two` の行より前に在る）
/// 3. **無いときは何も言わないこと**（**1 度目はファイルが無い**——
///    像は毎回作り直すので、**その起動で履歴について何も言っていない**）
///
/// # 値は像の語と当たらないものにしてある
///
/// `docs/coding-standards.md` の「判定が探す値は、像とログの語と当たらない
/// ものにする」。**`hist-one` / `hist-two` は像のどこにも無い。**
fn cmd_history_test(features: &[&str], expect_pass: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let mut all_features: Vec<&str> = vec!["history-test"];
    all_features.extend_from_slice(features);
    let kernel_elf = build_kernel_with_features(&workspace_root, &all_features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = all_features.join("-");
    let serial_log = workspace_root
        .join("target")
        .join(format!("history-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the history test")?;

    let deadline = Instant::now() + ZI_TEST_TIMEOUT;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if strip_ansi(&text).contains("script-done:") {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let context = if features.is_empty() {
        "history-test".to_string()
    } else {
        format!("history-test {}", features.join("+"))
    };
    let context = context.as_str();

    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    let stripped = strip_ansi(&serial);
    let restart_marker = "init: starting /bin/zash (restart 1 of 3)";
    let (first, second) = match stripped.find(restart_marker) {
        Some(at) => (&stripped[..at], &stripped[at..]),
        None => (stripped.as_str(), ""),
    };

    // **判定 1**——**辿って戻った行が走った。**
    let recalled_the_previous_run = second.lines().any(|line| line.trim() == "hist-two");
    // **判定 2**——**ファイルは古い順である。**
    let disk = disk_image_path(&esp_dir);
    let saved = debugfs_read(&disk, "/root/.zash_history")?;
    let saved_text = saved
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default()
        .into_owned();
    let oldest_first = match (saved_text.find("hist-one"), saved_text.find("hist-two")) {
        (Some(one), Some(two)) => one < two,
        _ => false,
    };
    // **判定 3**——**無いときは何も言わない。** **1 度目はファイルが無い**
    // （像は毎回作り直す）。
    let missing_is_silent = !first.contains("the history");

    println!("{context}: the previous run came back = {recalled_the_previous_run}");
    println!(
        "{context}: the file keeps the oldest line first = {oldest_first} \
         (the device says {saved_text:?})"
    );
    println!("{context}: a missing history said nothing = {missing_is_silent}");

    let passed = recalled_the_previous_run && oldest_first && missing_is_silent;
    if passed {
        println!("{context}: PASS");
        if expect_pass {
            Ok(())
        } else {
            bail!("{context}: the sabotage was NOT caught; every judgement still held")
        }
    } else {
        println!("{context}: FAILED");
        if expect_pass {
            bail!("{context}: FAILED")
        } else {
            println!("{context}: the sabotage was caught (this run is expected to fail)");
            Ok(())
        }
    }
}

/// Tab の補完の判定（TAB-1）。**1 回の起動で 5 つ見る。**
///
/// # 判定は 5 つである
///
/// 1. **候補が 1 本なら語を置き換えて空白を足す**（`ec` + Tab で
///    `echo tab-one` が走る）
/// 2. **共通接頭辞まで伸びる**（`r` + Tab で `rm` になり、`rmx` を打ったと
///    シェルが言う。**伸びなければ `rx` である**）
/// 3. **伸びなければ件数が出る**（`2 matches`）
/// 4. **もう一度 Tab を打つと一覧が出る**（`rm rmdir`）
/// 5. **`PATH` を変えると候補の源が変わる**（`export PATH=/data` の後に
///    `l` + Tab で `lines ` になり、シェルが `lines` を引けないと言う。
///    **控えていれば `ly` である**）
///
/// **重複は一覧で見る**——**`export PATH=/bin:/bin` の後の一覧が 2 本目で、
/// 落とさなければ `rm rmdir rm rmdir` である。** **件数では見ない**
/// ——**黙る破壊でも落ちてしまい、1 つの破壊が 2 本落とす形になる**（実測）。
///
/// # 走らせずに見る
///
/// **補完した語に字を足して `cannot run` にする。** **シェルの返事に語が
/// そのまま出るので、補完の結果が 1 行で読める**（`kernel/src/input.rs` の
/// FP の状態が保たれることを見る（B-a。`ADR-0058`）。
///
/// # 判定は 3 つで、決定に 1 対 1 で対応する
///
/// - **起こされた時点の XMM が 0 である**（決定 4）。**`/bin/fptest` を 2 回
///   起こし、2 回とも 0 であることを見る**——**1 回目が終わりに目印を残すので、
///   2 回目が汚れていれば既定値から始めていない。**
/// - **浮動小数点の足し上げが期待値と一致する**（決定 1）。**200 万回足すので、
///   その間にタイマが何度も食い込む。**
/// - **`spawn` を跨いで親の XMM が残る**（決定 2 の遠征の側）。
///
/// # 子が走ったことは合図である
///
/// **子が走らなければ 3 番目は主張にならない**（親の値が誰にも壊されない）。
/// **`(signal)` として出す**——**判定ではなく前提である。**
fn cmd_fp_test(features: &[&str], expect_pass: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let mut all_features: Vec<&str> = vec!["fp-test"];
    all_features.extend_from_slice(features);
    let kernel_elf = build_kernel_with_features(&workspace_root, &all_features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = all_features.join("-");
    let serial_log = workspace_root
        .join("target")
        .join(format!("fp-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the fp test")?;

    let deadline = Instant::now() + ZI_TEST_TIMEOUT;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if strip_ansi(&text).contains("script-done:") {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let context = if features.is_empty() {
        "fp-test".to_string()
    } else {
        format!("fp-test {}", features.join("+"))
    };
    let context = context.as_str();

    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    // **シェルの前後を分けない**——**`init` が起こした 1 回目はシェルより
    // 前に出る。** 他の台本の判定と違い、**ここは起動シーケンスの語と当たらない**
    // （`fp:` で始まる行はこの 2 本のプログラムしか出さない）。
    let stripped = strip_ansi(&serial);

    // **判定 1**——**起こされた時点の XMM が 0 である**（決定 4）。
    // **`init` が起こした 1 回目はシェルより前に出る**ので、全体から拾う。
    let starts: Vec<&str> = stripped
        .lines()
        .map(|line| line.trim())
        .filter(|line| line.starts_with("fp: xmm0 at start = "))
        .collect();
    // **3 本出る**——`init` が起こした `fptest`、その子の `fpchild`、
    // シェルから起こした `fptest` である。
    //
    // **子の 1 本が決定 4 の観測点である**——**親は XMM に目印を載せてから
    // 子を起こす。** **親の側では見えない**（親が終わるときに残した目印は、
    // 親を起こした側の復元が消す。**実測で、それに気づくまで判定が空振りした**）。
    let fresh_at_start = starts.len() == 3
        && starts
            .iter()
            .all(|line| line.ends_with("0x0000000000000000"));

    // **判定 2**——**足し上げが期待値と一致する**（決定 1）。
    // **200 万回 × 1.5 を 2 倍した値である**（`fptest.c` が整数で出す）。
    let sums: Vec<&str> = stripped
        .lines()
        .map(|line| line.trim())
        .filter(|line| line.starts_with("fp: sum = "))
        .collect();
    let sum_survived = sums.len() == 2 && sums.iter().all(|line| *line == "fp: sum = 6000000");

    // **判定 3**——**`spawn` を跨いで親の XMM が残る**（決定 2）。
    let after_spawn: Vec<&str> = stripped
        .lines()
        .map(|line| line.trim())
        .filter(|line| line.starts_with("fp: xmm0 after spawn = "))
        .collect();
    // **1 本目だけが主張を担う**——**`init` が起こした深さ 1 の回である。**
    // **2 本目はシェルから起こした深さ 2 の回で、子は深さの上限で断られる**
    // （`-EAGAIN`。実測。`MAX_EXCURSION_DEPTH` = 2）。**そちらは計器である。**
    let parent_kept_xmm0 = after_spawn.first().is_some_and(|line| {
        line.contains("0x1122334455667788") && line.contains("(child returned 0)")
    });

    // **判定 4**——**Ring 3 が浮動小数点の例外を上げても、カーネルは止まらない**
    // （`ADR-0058` で `#MF`(16) と `#XM`(19) を畳めるベクタへ入れた）。
    //
    // **観測できるのは `#MF` の側だけである**——**`#XM` は QEMU の TCG では
    // 上がらない**（実測。2026-09-07。**同じコードはホストで `SIGFPE` になる**）。
    // **`/bin/fpfault` は両方を試し、上がったほうで畳まれる。**
    let folded_the_fp_fault = stripped.contains("/bin/fpfault ended (Folded(16))");
    // **止まっていないことは、台本が最後まで進んだことで言う。**
    let script_finished = stripped.contains("script-done:");

    // **計器**——**TCG が `#XM` を配送しないことを、出力に残しておく。**
    let simd_did_not_fire = stripped.contains("the SIMD exception did not fire");

    // **合図**——**子が走っていなければ、判定 3 は何も主張していない。**
    let child_ran = stripped.matches("fpchild: clobbered xmm0").count() == 1;

    println!("{context}: (signal) the child ran and clobbered xmm0 = {child_ran}");
    println!(
        "{context}: a freshly started program sees xmm0 = 0 = {fresh_at_start} \
         (the lines were {starts:?})"
    );
    println!(
        "{context}: the floating-point sum survived the switches = {sum_survived} \
         (the lines were {sums:?})"
    );
    println!(
        "{context}: the parent kept xmm0 across spawn = {parent_kept_xmm0} \
         (the lines were {after_spawn:?})"
    );
    println!(
        "{context}: a floating-point exception folded the program instead of halting the \
         kernel = {folded_the_fp_fault} (the script ran to the end = {script_finished})"
    );
    println!(
        "{context}: (info) QEMU's TCG did not deliver #XM, so only #MF is observed here = \
         {simd_did_not_fire}"
    );

    if !child_ran {
        println!("{context}: FAILED");
        bail!("{context}: the child did not run, so the spawn judgement asserts nothing")
    }

    let passed = fresh_at_start
        && sum_survived
        && parent_kept_xmm0
        && folded_the_fp_fault
        && script_finished;
    if passed {
        println!("{context}: PASS");
        if expect_pass {
            Ok(())
        } else {
            bail!("{context}: the sabotage was NOT caught; every judgement still held")
        }
    } else {
        println!("{context}: FAILED");
        if expect_pass {
            bail!("{context}: FAILED")
        } else {
            println!("{context}: the sabotage was caught (this is the expected outcome)");
            Ok(())
        }
    }
}

/// 台本の doc）。
fn cmd_complete_test(features: &[&str], expect_pass: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let mut all_features: Vec<&str> = vec!["complete-test"];
    all_features.extend_from_slice(features);
    let kernel_elf = build_kernel_with_features(&workspace_root, &all_features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = all_features.join("-");
    let serial_log = workspace_root
        .join("target")
        .join(format!("complete-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the completion test")?;

    let deadline = Instant::now() + ZI_TEST_TIMEOUT;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if strip_ansi(&text).contains("script-done:") {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let context = if features.is_empty() {
        "complete-test".to_string()
    } else {
        format!("complete-test {}", features.join("+"))
    };
    let context = context.as_str();

    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    let stripped = strip_ansi(&serial);
    // **シェルが出た後だけを見る**（起動シーケンスでも `echo` は走る）。
    let after_shell = stripped
        .find("zash: ready")
        .map(|at| &stripped[at..])
        .unwrap_or("");

    // **判定 1**——**候補 1 本で空白まで足りた。**
    let single_completed = after_shell.lines().any(|line| line.trim() == "tab-one");
    // **判定 2**——**共通接頭辞まで伸びた。**
    let grew_to_the_common_prefix = after_shell.contains("zash: rmx:");
    // **判定 3 と 4**——**件数と一覧。** **件数は打った順に 2 本出る**
    // （1 本目が `PATH=/bin`、2 本目が `PATH=/bin:/bin` である）。
    let counts: Vec<&str> = after_shell
        .lines()
        .map(|line| line.trim())
        .filter(|line| line.ends_with(" matches"))
        .collect();
    let announced_the_count = counts.first() == Some(&"2 matches");
    // **一覧は打った順に 2 本出る**（1 本目が `PATH=/bin`、2 本目が
    // `PATH=/bin:/bin` である）。
    let lists: Vec<&str> = after_shell
        .lines()
        .map(|line| line.trim())
        .filter(|line| line.starts_with("rm rmdir"))
        .collect();
    let listed_on_the_second_tab = lists.first() == Some(&"rm rmdir");
    // **重複を落としたか**——**2 本目の一覧である。**
    //
    // **件数では見ない。** **件数で見ると、黙る破壊（件数を出さない側）でも
    // 落ちてしまい、1 つの破壊が 2 本落とす形になる**（実測。2026-09-05）。
    // **一覧は黙る破壊でも出るので、重複だけを見分けられる。**
    let dropped_the_duplicates = lists.get(1) == Some(&"rm rmdir");
    // **判定 5**——**`PATH` を変えると候補の源が変わる。**
    // **候補が 1 本なので空白が付く**——**続けて打った `y` は次の語になる。**
    // **シェルの返事に出るのは `lines` である**（実測。2026-09-05）。
    // **起動時の `PATH` を控えていると `ls` と `less` で伸びず、`ly` になる。**
    let followed_the_path = after_shell.contains("zash: lines:");

    println!("{context}: a single candidate completed with a space = {single_completed}");
    println!("{context}: the word grew to the common prefix = {grew_to_the_common_prefix}");
    println!(
        "{context}: a tab that did not grow announced the count = {announced_the_count} \
         (the count lines were {counts:?})"
    );
    println!(
        "{context}: the second tab listed the candidates = {listed_on_the_second_tab} \
         (the lists were {lists:?})"
    );
    println!(
        "{context}: a duplicated PATH element listed each name once = {dropped_the_duplicates}"
    );
    println!("{context}: the candidates followed PATH = {followed_the_path}");

    let passed = single_completed
        && grew_to_the_common_prefix
        && announced_the_count
        && listed_on_the_second_tab
        && dropped_the_duplicates
        && followed_the_path;
    if passed {
        println!("{context}: PASS");
        if expect_pass {
            Ok(())
        } else {
            bail!("{context}: the sabotage was NOT caught; every judgement still held")
        }
    } else {
        println!("{context}: FAILED");
        if expect_pass {
            bail!("{context}: FAILED")
        } else {
            println!("{context}: the sabotage was caught (this run is expected to fail)");
            Ok(())
        }
    }
}

fn cmd_zi_test(features: &[&str]) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let mut all_features: Vec<&str> = vec!["zi-test"];
    all_features.extend_from_slice(features);
    let kernel_elf = build_kernel_with_features(&workspace_root, &all_features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = all_features.join("-");
    let serial_log = workspace_root
        .join("target")
        .join(format!("zi-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the zi test")?;

    // **台本の最後の出力が出るまで待つ。** 上限つき。
    //
    // **本数で待たない。** 判定行の本数で切ると、**台本を出し切る前に
    // QEMU を止めてしまう**（実測でそうなった——`x` の削除が届く前に
    // 落として、`x deleted a byte = false` になった）。
    // **台本の最後は `cat` の読み戻し**（zi-d-2）だが、`cat` は起動シーケンス
    // でも走るので、**その行では早く切れる**（実測）。**シェルが `cat` を
    // 終えたことを、プロンプトの反響で見る。**
    // **台本の最後の判定行が出るまで待つ（e-3）。**
    //
    // **以前はプロンプトの反響（`zaytos$ /bin/cat /data/lines`）を待っていた**が、
    // **あれは `cat` が走る前に出る**ので、**その後に置いた観測点が間に合う保証が
    // 無い**（`kernel/src/input.rs` の `OBSERVE_AFTER_ALT`）。
    //
    // **次に、代替画面の観測の行を合図にした。** **DIR-1b でその観測点を
    // 台本の途中へ移したところ、待ちが途中で切れた**（実測。**`tail` と `rm` が
    // 走る前に QEMU を止めていた**）。
    //
    // **合図と主張を分けた。** **`script-done:` は何も主張しない観測点で、
    // 台本の末尾にだけ在る**（`crate::console::probe` の `ScriptDone`）。
    let done_marker = "script-done:";
    let started_waiting = Instant::now();
    let deadline = started_waiting + ZI_TEST_TIMEOUT;
    let mut finished_after = None;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        // **色の列を落としてから探す（ES-d）。** [`strip_ansi`] の doc。
        if strip_ansi(&text).contains(done_marker) {
            finished_after = Some(started_waiting.elapsed());
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let context = if features.is_empty() {
        "zi-test".to_string()
    } else {
        format!("zi-test {}", features.join("+"))
    };
    let context = context.as_str();

    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    // **カーソルの推移を判定行から拾う。** `row` の列がそのまま台本の答えである。
    //
    // **矢印と `hjkl` を札で分ける（ES-d の手当て）**——[`parse_zi_cursor_rows`]。
    let arrow_rows = parse_zi_cursor_rows(&serial, "arrow");
    let move_rows = parse_zi_cursor_rows(&serial, "move");

    let started = serial.contains("zi: ready");
    // **上下でカーソルが動いた（zi-a で zi-d へ委ねた分担の条件）。**
    // 台本は下・下・上を送るので、`row` は 0 -> 1 -> 2 -> 1 と動く。
    let arrows_moved = arrow_rows.windows(2).any(|pair| pair[1] > pair[0])
        && arrow_rows.windows(2).any(|pair| pair[1] < pair[0]);
    // **hjkl でも動いた。** 台本は j j k k を送るので、上下の後にもう一度
    // 増えて減る推移が出る。**両方の経路が同じ動きを作ることの主張である。**
    let distinct_rows = {
        let mut seen: Vec<u32> = move_rows.clone();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    };
    let hjkl_moved = distinct_rows >= 3;
    // **挿入と削除がバッファへ効いた。** 台本は `i Z Y Esc x` なので、
    // 桁が 2 つ進んでから戻り、削除で 1 つ減る。
    //
    // **本数で見る（ES-d の手当て）。** **在るかどうかで見ていたので、
    // 1 字目を落とす破壊（`zi-insert-drop-first`）が通っていた**——
    // **残る 1 字が `typed` を出すので、`contains` は真のままである。**
    // **往復の判定も捕まえない**——`zi` の再描画と `cat` の読み戻しは
    // どちらも落とした後の内容なので、一致してしまう
    // （`docs/verification-coverage.md`）。
    // **台本が送る挿入は 3 字である**（`kernel/src/input.rs` の `SCRIPT` の
    // `iZY` と `aQ`）。**この数は台本の写しで、台本を変えたらここも変わる。**
    //
    // **実際に変わった（e-4）。** **台本へ `a` の挿入を 1 字足したとき、
    // ここが 2 のままだったので、1 字落とす破壊が捕まらなくなった**
    // （落としても 2 字残るため）。**`--full` が「破壊が捕まらない」と
    // 出して分かった**——**写しは、写した先が変わった瞬間に古くなる。**
    //
    // **数え直した（H-b-2）。** **数えているのは台本ぜんたいの `typed` の本数で、
    // `/data/lines` の回だけではない**——**実測で 16 本である**
    // （`fresh` の `NEW` が 3、`lines` の `ZY` と `Q` が 3、`edited` の
    // `ab` `cd` `X` が 5、`joined` の `ab` `cd` が 4、`big` の `Z` が 1）。
    //
    // **下限は 3 のままにしてある。** **この判定は「挿入がバッファへ届いた」
    // までしか言わない**——**1 字落とす破壊を捕まえているのは往復のほうである**
    // （`zi-insert-drop-first` の項。`docs/verification-coverage.md`）。
    // **本数を実測値へ固定すると、台本を触るたびに直す作業が増えるだけで、
    // 捕まえる力は増えない。**
    let typed_events = serial
        .lines()
        .filter(|line| line.contains("zi: cursor") && line.trim_end().ends_with("typed"))
        .count();
    let typed = typed_events >= 3;
    // **札で見る（2026-09-04 に直した）。** **以前は `serial.contains("normal")`
    // だった**——**起動ログの `test hooks:` の行に `normal` が入っており、
    // どの構成でも真になっていた**（実測。既定の構成は
    // `(this is a normal build)`、破壊の構成は `do not treat this run as a
    // normal result`）。**判定の当たり先がずれる族で、値が短いほど起きやすい**
    // （`docs/troubleshooting.md` の 2026-09-04 の 2 件）。
    // **`delete` は当たっていなかったが、同じ形なので一緒に締めた。**
    let tagged = |tag: &str| {
        serial
            .lines()
            .any(|line| line.contains("zi: cursor") && line.trim_end().ends_with(tag))
    };
    let deleted = tagged("delete");
    let back_to_normal = tagged("normal");

    // **保存が要求した量を書いたこと（zi-d-2）。**
    //
    // **`:w` は open の時点で長さ 0 へ切る**ので、**書けなければ内容が
    // 失われている。** `zi` が判定行に量を並べるので、一致を見る。
    //
    // **この判定は、下の往復の判定が在って初めて意味を持つ。**
    // **単独では「書いたつもり」を通す**——実測で、`zi-write-skip-body`
    // （中身を書かずに閉じる破壊）は**要求 0 に対して 0 を書くので
    // `match=true` になる。** 量だけを見ていたら捕まらなかった。
    // **往復（`cat` の読み戻し）を削るなら、この判定も守っていない。**
    // **削る者がその関係に気づけるよう、ここに書いておく。**
    let saved = serial
        .lines()
        .any(|line| line.contains("zi: saved bytes") && line.contains("match=true"));

    // **`cat` の読み戻しが、`zi` が編集した内容と一致すること（zi-d-2）。**
    //
    // **期待値をこちらが持たない。** `zi` の最後の再描画に編集後の行が
    // 並んでいるので、**そこから読み取って `cat` の出力と突き合わせる**
    // （「期待値は定数で持たず外の道具から導く」）。
    // **`/data/lines` の回に区切る（zi-f）。**
    //
    // **台本の末尾に `zi` の回がもう 1 つ増えた**ので、**区切らないと
    // 「最後の再描画」があちらを指し、`/data/lines` の読み戻しと
    // 突き合わなくなる**（実測で踏んだ）。
    // **台本を変えるときは、台本に寄りかかっている判定を数え直すこと。**
    let lines_session = serial
        .split("/bin/tail /data/lines")
        .next()
        .unwrap_or(&serial);
    // **画面の実物から読む（PERF-g）。**
    //
    // **シリアルの列から組み立て直していた**（`parse_zi_last_redraw`）が、
    // **`zi` が編集で全面を描き直さなくなったので成り立たなくなった**
    // ——**部分の描き直しは、前の全面の出力と混ざる。**
    // **`docs/verification-coverage.md` の一覧で「条件つきで危ない」と
    // 印を付けてあった項目で、印のとおりに壊れた。**
    //
    // **画面を読む形にすれば、描き方に依らない。** **観測点は `:w` の直前に
    // 置いてある**（`kernel/src/input.rs` の台本）。
    let edited_lines: Vec<String> = serial
        .lines()
        .filter_map(|line| line.split("screen-text: row ").nth(1))
        .filter_map(|rest| rest.split_once("says "))
        .map(|(_, value)| {
            value
                .trim()
                .trim_end_matches('\r')
                .trim_matches('"')
                .to_string()
        })
        .take_while(|row| !row.is_empty())
        .collect();
    let readback = parse_cat_readback(lines_session, edited_lines.len());
    let roundtrip = !edited_lines.is_empty() && edited_lines == readback;

    // **保存が装置へ届いたこと（P-c-1）。**
    //
    // **`zi` の `:w` は開いて書いて閉じる。** **閉じたときに像が装置へ書き戻る。**
    // **据えられていなければ黙って飛ばされる**（あの形は起動シーケンスの中で
    // 走るプログラムのために要る）ので、**据え忘れは黙る。** **この判定が塞ぐ。**
    //
    // **破壊は `virtio-skip-install-test`**（据えない形）。**その形でしか
    // 落ちない**——`zi` の他の破壊は開いて閉じる経路を通るので、書き戻しは起きる。
    let save_reached_the_device = serial.contains("user-flush: /bin/zi wrote the image back");

    // **代替画面から戻る描き直しが、塗った色のままの空白を飛ばしている
    // こと（PERF-h）。**
    //
    // # なぜ数で見るのか
    //
    // **サイクルは揺れる**（機械と負荷で動く）。**描いたセルの数は
    // 揺れない**——**台本が同じなら同じ画面になり、同じ数になる。**
    // **判定は数で行い、サイクルは (info) の側へ置く**（運用者の指示）。
    //
    // # 期待値を定数で持たない
    //
    // **上限は「画面のセル数 x 描き直した回数」で、どちらもシリアルから
    // 読む**——`zi: winsize rows=R columns=C` と `repaints=N` である。
    // **飛ばしているなら必ずこれより少ない。**
    // **飛ばさない形（`repaint-blank-cells-test`）では、ちょうど等しくなる。**
    //
    // # 0 も落とす
    //
    // **1 つも描いていなければ、絵が出ていない。** **`> 0` が要る**
    // ——**「全部飛ばす」も上限より少ないので、上だけでは通ってしまう。**
    let screen_size = serial
        .lines()
        .find_map(|line| line.split("zi: winsize rows=").nth(1))
        .and_then(|rest| {
            let mut parts = rest.split(" columns=");
            let rows: u64 = parts.next()?.trim().parse().ok()?;
            let columns: u64 = parts
                .next()?
                .split_whitespace()
                .next()?
                .trim_end_matches('\r')
                .parse()
                .ok()?;
            Some(rows * columns)
        });
    let repaint_field = |name: &str| -> Option<u64> {
        serial
            .lines()
            .rfind(|line: &&str| line.contains("screen-cost: "))
            .and_then(|line| line.split(&format!("{name}=")).nth(1))
            .and_then(|rest| {
                rest.split_whitespace()
                    .next()?
                    .trim_end_matches('\r')
                    .parse()
                    .ok()
            })
    };
    let repaints = repaint_field("repaints");
    let repaint_cells = repaint_field("repaint_cells");
    let repaint_skips_blank_cells = match (screen_size, repaints, repaint_cells) {
        (Some(size), Some(times), Some(cells)) if times > 0 => cells > 0 && cells < size * times,
        _ => false,
    };

    // **画面の実物で色が出ていること（ES-d）。**
    //
    // **判定を出すのはカーネルである**（`kernel/src/console/probe.rs`）——
    // **色を出す利用者は Ring 3 に居て、画面を読み戻せない。**
    // **台本の中の観測点で、そのつど画面を見ている。**
    //
    // **上の判定群とは層が違う。** あちらは `zi` の内部状態で、
    // **こちらはバックバッファのピクセルである。**
    let judged = |marker: &str| {
        serial
            .lines()
            .any(|line| line.contains(marker) && line.contains("= true"))
    };
    let prompt_colored = judged("the zash prompt name is drawn in its own color");
    // **色が記号へ漏れていないこと（zi-e 前の色替え）。** 連なりの直後の
    // セルが既定前景で、字が在ることを見る。
    let prompt_symbol_plain = judged("the prompt symbol kept the default color");

    // **`ioctl(TIOCGWINSZ)` が答えた大きさが、カーネルの画面と一致すること（e-1）。**
    //
    // **期待値をホストが持たない。** カーネルは自分の `Console` から読んだ値を
    // `screen-size:` の行に出し、`zi` は `ioctl` を通って受け取った値を
    // `zi: winsize` の行に出す。**両側の数字を突き合わせる。**
    //
    // **源は独立している**——片方は画面を持っている側、もう片方は
    // システムコールの戻り値である。**入れ替えれば食い違う**
    // （画面は 160x50 で正方形ではない）。
    let kernel_geometry = serial.lines().find_map(|line| {
        let rest = line.split("screen-size: the console is ").nth(1)?;
        let rows = rest.split("rows=").nth(1)?.split(' ').next()?.to_string();
        let columns = rest
            .split("columns=")
            .nth(1)?
            .split(' ')
            .next()?
            .to_string();
        Some((rows, columns))
    });
    let zi_geometry = serial.lines().find_map(|line| {
        let rest = line.split("zi: winsize ").nth(1)?;
        let rows = rest.split("rows=").nth(1)?.split(' ').next()?.to_string();
        let columns = rest
            .split("columns=")
            .nth(1)?
            .trim_end_matches('\r')
            .to_string();
        Some((rows, columns))
    });
    let winsize_agrees =
        kernel_geometry.is_some() && zi_geometry.is_some() && kernel_geometry == zi_geometry;
    let status_colored = judged("the zi status line is drawn in its own color");
    let status_followed_mode = judged("the zi status line followed the mode");

    // **Esc 1 バイトで、次の入力を待たずに前のモードへ戻ること（e-2）。**
    //
    // **台本は `i Z Y` の後に Esc を 1 バイト送り、そこで「休み」を挟む**
    // （`kernel/src/input.rs` の `SCRIPT_PAUSE`）。**休みは `read` を空に
    // するので、`zi` は「入力が途切れた」を見る。** 直後の観測点で札を読む。
    //
    // **札の文字列を写さない。** **3 つ目が 1 つ目と同じで、2 つ目と違う**
    // ことを見る——ノーマル、インサート、ノーマル、の並びである。
    let status_labels: Vec<&str> = serial
        .lines()
        .filter_map(|line| line.split("the zi status line says ").nth(1))
        .map(|rest| rest.trim().trim_end_matches('\r'))
        .collect();
    let esc_settled_at_once = status_labels.len() >= 3
        && status_labels[2] == status_labels[0]
        && status_labels[2] != status_labels[1];

    // **画面のカーソルが、バッファのカーソルへ追従していること**
    // （PERF-b の後。**回帰で気づいた**）。
    //
    // **突き合わせるのは `zi` の診断行である**——**あちらは `row` / `col` /
    // `top` を出しており、窓の中の位置は `row - top`、桁は `col` である。**
    // **期待値をホストが持たない**——**どちらも実測の値で、計算だけをここでする。**
    //
    // **観測点は台本の `lh`（ノーマルモードの移動）の直後に置いてある。**
    let cursor_cell = serial
        .lines()
        .find_map(|line| line.split("screen-cursor: the cursor cell is ").nth(1))
        .and_then(|rest| {
            let mut row = None;
            let mut column = None;
            for field in rest.trim().trim_end_matches('\r').split_whitespace() {
                match field.split_once('=') {
                    Some(("row", value)) => row = value.parse::<usize>().ok(),
                    Some(("column", value)) => column = value.parse::<usize>().ok(),
                    _ => {}
                }
            }
            Some((row?, column?))
        });
    // **観測点の手前で最後に出た診断行を取る。**
    let buffer_cursor = serial
        .lines()
        .take_while(|line| !line.contains("screen-cursor:"))
        .filter(|line| line.contains("zi: cursor (buffer state"))
        .last()
        .and_then(|line| {
            let mut row = None;
            let mut col = None;
            let mut top = None;
            for field in line.split_whitespace() {
                match field.split_once('=') {
                    Some(("row", value)) => row = value.parse::<usize>().ok(),
                    Some(("col", value)) => col = value.parse::<usize>().ok(),
                    Some(("top", value)) => top = value.parse::<usize>().ok(),
                    _ => {}
                }
            }
            Some((row?.checked_sub(top?)?, col?))
        });
    let cursor_followed = cursor_cell.is_some() && cursor_cell == buffer_cursor;

    // **描画の費用（PERF-e）。** **窓が動く 1 行の移動の前後で撮ってある。**
    // **主張しない**——**層ごとの数を差で出すだけである。**
    let zi_costs: Vec<Vec<u64>> = serial
        .lines()
        .filter_map(|line| line.split("screen-cost: ").nth(1))
        .map(|rest| {
            rest.split_whitespace()
                .filter_map(|field| field.split('=').nth(1))
                .filter_map(|value| value.trim_end_matches('\r').parse::<u64>().ok())
                .collect()
        })
        .collect();
    // **60 回の `j` の全体（PERF-f の測定）。** **1 打鍵あたりの実時間を出す。**
    // **観測は 3 つある**——**走り始め、60 回目の直前、60 回目の直後である。**
    let zi_insert_cost: Option<Vec<u64>> = match (zi_costs.first(), zi_costs.get(1)) {
        (Some(before), Some(after)) if before.len() == after.len() => Some(
            before
                .iter()
                .zip(after)
                .map(|(x, y)| y.saturating_sub(*x))
                .collect(),
        ),
        _ => None,
    };
    let zi_run_cost: Option<Vec<u64>> = match (zi_costs.get(2), zi_costs.get(4)) {
        (Some(before), Some(after)) if before.len() == after.len() => Some(
            before
                .iter()
                .zip(after)
                .map(|(x, y)| y.saturating_sub(*x))
                .collect(),
        ),
        _ => None,
    };
    let zi_move_cost: Option<Vec<u64>> = match (zi_costs.get(3), zi_costs.get(4)) {
        (Some(before), Some(after)) if before.len() == after.len() => Some(
            before
                .iter()
                .zip(after)
                .map(|(x, y)| y.saturating_sub(*x))
                .collect(),
        ),
        _ => None,
    };

    // **代替画面バッファ（e-3）。** **抜けた後に元の画面が戻っていることを、
    // 画面の実物で見る。** カーネルが入る前のピクセルを控え、戻った後に
    // 画面じゅうから同じインクを探して突き合わせている
    // （`kernel/src/console/probe.rs`。**VIEW-b で行番号を控える形をやめた**）。
    let screen_came_back = judged("the screen before the alternate screen came back");

    // **2 本立て（e-4）。** 状態行が下から 2 行目に在ること。
    // **`ioctl(TIOCGWINSZ)` が答えた行数を `zi` が実際に使っていることの
    // 主張である**（行番号はカーネルが画面から取る）。
    let status_at_bottom = judged("the zi status line sits on the second-to-last row");
    // **コマンド行に、打っている途中が出ていること。**
    // **台本の `:w` の直後に観測している**ので、最下行は `:w` である。
    // **`:` を含む形であることまで見る**——`w` だけでは、コマンド行ではなく
    // 本文へ出ていても通る。
    let command_line = serial
        .lines()
        .find_map(|line| line.split("screen-command: ").nth(1))
        .map(|rest| rest.trim().trim_end_matches('\r').to_string());
    let command_line_echoes = command_line
        .as_deref()
        .is_some_and(|line| line.contains("\":w\""));

    // **報せがコマンド行に出ていること（e-5）。**
    //
    // **台本は変更を持ったまま `:q` を打つ**ので、`zi` は断る。
    // **断った理由が最下行に出ていることを、画面の実物で見る。**
    // **`STDERR` へ出していたものを移した**——**`zi` は代替画面に居るので、
    // あちらは「使う人が見る画面」ではない。**
    let message_line = serial
        .lines()
        .find_map(|line| line.split("screen-message: ").nth(1))
        .map(|rest| rest.trim().trim_end_matches('\r').to_string());
    let message_shown = message_line
        .as_deref()
        .is_some_and(|line| line.contains("unsaved changes"));

    // **新しいファイルを作れること（e-5。`O_CREAT`）。**
    //
    // **`ls` を前後で撮り、後にだけ在ることを見る**——**源は `zi` ではない**
    // （`ls` はカーネルの `getdents64` を通って像を読む）。
    // **`cat` の読み戻しは、`zi` が書いた中身が像に入ったことを見る。**
    // **`ls /data` は 1 行に 1 つ出す**ので、**行がちょうど名前と等しいか**を見る
    // （`zi` の状態行にも `/data/fresh` が出るが、あちらは 1 行の一部である）。
    let plain = strip_ansi(&serial);
    // **プロンプトを目印にしない**——**観測の出力がプロンプトと反響の間へ
    // 割り込むことがある**（1 回目がそうなる。`kernel/src/console/probe.rs`）。
    let mut listings = plain.split("/bin/ls /data");
    let _boot = listings.next();
    let after_first = listings.next().unwrap_or("");
    let after_second = listings.next().unwrap_or("");
    // **3 回目は `rm` の後である（DIR-1b）。**
    let after_third = listings.next().unwrap_or("");
    // **1 回目の一覧は、`zi` を起こす手前までである。**
    let first_listing = after_first.split("/bin/zi").next().unwrap_or("");
    // **2 回目の一覧は、`cat` を起こす手前までである。**
    let second_listing = after_second.split("/bin/cat").next().unwrap_or("");
    let lists_fresh = |segment: &str| {
        segment
            .lines()
            .any(|line| line.trim_end_matches('\r') == "fresh")
    };
    let created_file_appeared = !lists_fresh(first_listing) && lists_fresh(second_listing);

    // **`rm` が消したこと（DIR-1b）。**
    //
    // **同じ `ls` を 3 回撮っている**——**無い / 在る / また無い。**
    // **「無い」を 1 回だけ見ると、そもそも作られなかった形と区別できない。**
    // **`created_file_appeared` が「在る」を主張しているので、
    // ここは「また無い」だけを見ればよい。**
    let removed_file_disappeared = !lists_fresh(after_third);

    // **`tail` が末尾を出したこと（DIR-1b。`lseek` の利用者）。**
    //
    // **期待値をホストが持たない**——**`cat` が出した本文の末尾と突き合わせる。**
    // **`tail` は 12 バイトだけ出す**ので、**`cat` の出力と同じにはならない**
    // ——**同じなら跳んでいない。**
    //
    // **`cat /data/lines` の出力は、`zi` が編集した後の 4 行である。**
    // 直前の `cat` の出力を取り、その末尾 12 バイトを期待にする。
    //
    // **プログラムの出したものだけを取り出す。** **シリアルには判定行と
    // ログが混ざる**ので、`[` で始まる行とプロンプトを落とす。
    // **`/data/lines` に空行が無い**ので、空行も落として差し支えない。

    let tail_output = program_output(
        plain
            .split("/bin/tail /data/lines")
            .nth(1)
            .unwrap_or("")
            .split("/bin/rm")
            .next()
            .unwrap_or(""),
    );
    let cat_output = program_output(
        plain
            .split("/bin/cat /data/lines")
            .nth(1)
            .unwrap_or("")
            .split("/bin/tail")
            .next()
            .unwrap_or(""),
    );
    let tail_matches_the_end_of_cat = !cat_output.is_empty()
        && !tail_output.is_empty()
        && cat_output.ends_with(&tail_output)
        && tail_output.len() < cat_output.len();
    // **読み戻しの範囲を切ってから見る（2026-09-04 に締めた）。**
    // **以前はシリアル全体から「まるごと `NEW` の行」を探していた**
    // ——**当たってはいなかったが、`normal` が当たったのと同じ形である**
    // （3 字の値をログ全体から探す）。**`cat /data/fresh` の出力だけを見る。**
    let fresh_read_back = after_second
        .split("/bin/cat /data/fresh")
        .nth(1)
        .unwrap_or("")
        .split("/bin/zi")
        .next()
        .unwrap_or("");
    let fresh_content = fresh_read_back
        .lines()
        .any(|line| line.trim_end_matches('\r') == "NEW");

    // **ディレクトリの一巡（DIR-1c）。**
    //
    // **`mkdir` → `touch` → `ls` → `cat` → 空でない `rmdir`（断られる）→
    // `rm` → `rmdir` → `ls` を、台本が順に打っている。**
    //
    // **3 つを見る。**
    let after_mkdir = plain.split("/bin/mkdir /tmp/box").nth(1).unwrap_or("");

    // **(1) 作ったディレクトリに `touch` したファイルが `ls` で見えること。**
    let listing_in_box = program_output(
        after_mkdir
            .split("/bin/ls /tmp/box")
            .nth(1)
            .unwrap_or("")
            .split("/bin/cat")
            .next()
            .unwrap_or(""),
    );
    // **`.` と `..` も出る**（`ls` はそのまま出す。実測）。**3 つが揃うことを見る**
    // ——**`note` を含むだけでは、`.` と `..` が書けていない形が通ってしまう。**
    let made_directory_holds_the_file =
        listing_in_box.lines().collect::<Vec<_>>() == [".", "..", "note"];

    // **(2) 空でない `rmdir` が断られること。**
    //
    // **`rmdir` の断り書きを見る**（あれは `STDERR` へ出す）。
    let rmdir_refused = after_mkdir.contains("rmdir: cannot remove /tmp/box");

    // **(3) 一巡の後、`/tmp` に `box` が残っていないこと。**
    let listing_of_tmp = program_output(
        after_mkdir
            .split("/bin/ls /tmp\n")
            .nth(1)
            .unwrap_or(after_mkdir.rsplit("/bin/ls /tmp").next().unwrap_or("")),
    );
    let directory_removed = !listing_of_tmp.lines().any(|line| line.trim() == "box");

    // **(4) 失敗したのは、断られた `rmdir` の 1 回だけであること。**
    //
    // **`cat` は空のファイルを読むので何も出さない。** **単独の判定を
    // 持たせる代わりに、一巡ぜんたいで数える**——**`mkdir`・`touch`・`cat`・
    // `rm`・2 回目の `rmdir` がどれか 1 つでも失敗すれば、この数が増える。**
    let failures_in_the_round = after_mkdir.matches("zash: exit status ").count();
    let only_the_refused_rmdir_failed = failures_in_the_round == 1;

    // **`brk` が取った分を返したこと（H-a。ADR-0044 の到達条件）。**
    //
    // **`syscall-test` は 2 ページ伸ばしてから元へ縮める**（あちらの asm の
    // 62..67 番）。**取った数と返した数が一致していなければ、縮めたつもりで
    // 返っていない。**
    //
    // **空きフレームの全体は見ない**——**子を起こすので、子の空間のフレームが
    // 隔離へ入り、まだ空きへ戻っていない**（実測で 44 フレームの差。
    // `kernel/src/userland.rs` の `Heap` の doc）。
    let heap_line = plain
        .lines()
        .find(|line| line.contains("user-heap: syscall-test had brk take"))
        .unwrap_or("");
    let heap_numbers: Vec<u32> = heap_line
        .split_whitespace()
        .filter_map(|word| word.parse::<u32>().ok())
        .collect();
    // **拾うのは「取った数」と「返した数」の 2 つだけである。**
    let brk_returned_what_it_took =
        heap_numbers.len() >= 2 && heap_numbers[0] == heap_numbers[1] && heap_numbers[0] > 0;

    // **`zi` が取った分を返したこと（H-b-1）。**
    //
    // **`zi` は入れ物をヒープから取るようになった**（`kernel/userland/zi.rs`）。
    // **終わる道はどれも [`userlib::heap::release`] を通る**ので、
    // **取った数と返した数は一致するはずである。**
    //
    // **上の `syscall-test` の判定とは別に置く。** あちらは `brk` そのものの
    // 検算（asm で 2 ページ伸ばして縮める）で、**こちらは「本物の利用者が
    // 返し忘れていないこと」である。** **`syscall-test` が緑でも、
    // `zi` が返し忘れていれば、フレームは減り続ける。**
    //
    // **台本は `zi` を 3 回起こす**（`/data/lines`・`/data/fresh`・
    // `/data/edited`）。**回数を写さない**——**1 回でも取り忘れ・返し忘れが
    // あれば落ちる形にする。** **`> 0` も要る**——**取っていなければ
    // 「0 と 0」で一致してしまい、ヒープを使わなくなった形が通る。**
    let zi_heap_lines: Vec<&str> = plain
        .lines()
        .filter(|line| line.contains("user-heap: /bin/zi had brk take"))
        .collect();
    let zi_heap_pairs: Vec<(u32, u32)> = zi_heap_lines
        .iter()
        .map(|line| {
            let numbers: Vec<u32> = line
                .split_whitespace()
                .filter_map(|word| word.parse::<u32>().ok())
                .collect();
            (
                numbers.first().copied().unwrap_or(0),
                numbers.get(1).copied().unwrap_or(0),
            )
        })
        .collect();
    let zi_returned_what_it_took = !zi_heap_pairs.is_empty()
        && zi_heap_pairs
            .iter()
            .all(|(took, gave)| took == gave && *took > 0);

    // **1 回の `brk` が 2 ページより大きく伸ばせること（H-b-2）。**
    //
    // **`syscall-test` は 2 ページちょうどしか伸ばさない**（あちらの asm）。
    // **`zi` は開くファイルの大きさから容量を決める**ので、**大きいファイルの
    // 回では 1 回の要求が 2 ページを越える。** **越えた回が 1 つも無ければ、
    // 複数ページを写す道は 2 ページまでしか通っていないことになる。**
    //
    // **数を写さない**——**「2 より大きい」だけを見る。** 容量の決め方を
    // 変えれば実際の値は動くが、**主張は「2 ページを越える要求が通る」である。**
    let zi_largest_take = zi_heap_pairs
        .iter()
        .map(|(took, _)| *took)
        .max()
        .unwrap_or(0);
    let brk_grew_past_two_pages = zi_largest_take > 2;

    // **行頭の Backspace が前の行と繋げたこと（H-b-2。7-3 で塞いだ穴）。**
    //
    // **b-1 まで、この経路は台本が 1 度も通らなかった**
    // （`docs/verification-coverage.md` の「検査なし」）。**運用者の目視では
    // 通っていたが、判定が無かった。** **台本を触る b-2 で足した。**
    //
    // **台本は `ab` / `cd` の 2 行を作り、2 行目の行頭で Backspace を打つ。**
    // **1 行の `abcd` になるはずである。**
    let joined_output = program_output(
        plain
            .split("/bin/cat /data/joined")
            .nth(1)
            .unwrap_or("")
            .split("/bin/cat /data/big")
            .next()
            .unwrap_or(""),
    );
    let joined_lines_seen: Vec<&str> = joined_output.lines().collect();
    let backspace_joined_the_lines = joined_lines_seen == ["abcd"];

    // **上限が外れたこと（H-b-2）。**
    //
    // **像に `/data/big` を置いた**——**100 行**（b-1 までの上限は 64 行）と、
    // **200 バイトの行 1 本**（b-1 までの 1 行の上限は 128 バイト）。
    //
    // **期待値をホストが持たない。** **編集の前と後で `cat` を撮り、
    // 差が編集の分だけであることを見る**——**像の中身を定数として持たない。**
    // **`64` と `128` は像の写しではなく、b-1 まで在った上限そのものである**
    // ——**「その上限を越えている」ことがこの判定の言いたいことである。**
    let big_before = program_output(
        plain
            .split("/bin/cat /data/big")
            .nth(1)
            .unwrap_or("")
            .split("/bin/zi /data/big")
            .next()
            .unwrap_or(""),
    );
    // **区切りは次の回の手前である（ADR-0046 で台本が伸びた）。**
    // **`script-done` で切っていたが、後ろに `/nope/x` の回が付いた**ので、
    // **そのままだとその回の出力まで飲み込む**（`/data/lines` と `edited` で
    // 既に 2 度踏んでいる形である。**台本を変えたら、台本に寄りかかっている
    // 判定を数え直すこと**）。
    let big_after = program_output(
        plain
            .split("/bin/cat /data/big")
            .nth(2)
            .unwrap_or("")
            .split("/bin/zi /nope/x")
            .next()
            .unwrap_or(""),
    );
    let big_lines_before: Vec<&str> = big_before.lines().collect();
    let big_lines_after: Vec<&str> = big_after.lines().collect();
    let big_passes_the_old_line_limit = big_lines_before.len() > 64;
    let big_passes_the_old_length_limit = big_lines_before.iter().any(|line| line.len() > 128);
    // **台本は先頭の行の行頭へ `Z` を 1 字入れる。** 他の行は変わらない。
    let big_round_trip = big_lines_before.len() > 1
        && big_lines_after.len() == big_lines_before.len()
        && big_lines_after[0] == format!("Z{}", big_lines_before[0])
        && big_lines_after[1..] == big_lines_before[1..];

    // **窓が動いたこと（VIEW-a）。画面の実物で見る。**
    //
    // **バッファではない。** **`zi` は `top` を診断行に出しているが、
    // それはバッファ側である**——**H-b-2 で捕まえられなかったのは、
    // まさにバッファしか見ていなかったからである。**
    //
    // **台本は `/data/big` を開き、窓を動かす前と後で 2 回観測する**
    // （`kernel/src/console/probe.rs` の `observe_zi_window`）。
    //
    // **期待値をホストが持たない。** **2 つが違うことと、後のほうが
    // `cat` の出した並びの中で後ろに在ることを見る**——**像の中身も、
    // 窓の高さも、動いた量も、写していない。**
    //
    // **読むのは画面の行 0 である（ADR-0046 で戻した）。** **VIEW-a では
    // 行 1 を読んでいた**——**診断行が行 0 を上書きしていたための迂回で、
    // 診断の出口を画面から外したので要らなくなった。**
    //
    // **観測は 3 つである（ADR-0046 で `k` を 60 足した）。**
    // **下へ 60、上へ 60 で、3 つ目は 1 つ目へ戻る**——**上へ戻る形が
    // QEMU で一度も通っていなかった。**
    let window_rows: Vec<&str> = serial
        .lines()
        .filter_map(|line| {
            line.split("screen-window: the top row of the window says ")
                .nth(1)
        })
        .map(|rest| rest.trim().trim_end_matches('\r').trim_matches('"'))
        .collect();
    // **突き合わせる相手は編集の後の像である（ADR-0046）。**
    //
    // **VIEW-a では編集の前を見ていた。** **行 1 を読んでいたので、
    // 編集した行（先頭）に当たらなかっただけである。** **行 0 を読むように
    // なると、そこは `Z` を入れた当の行で、編集の前の並びには無い。**
    //
    // **後の像で見るのが正しい**——**窓を動かしたのは編集の後であり、
    // 画面に出ていたのは保存された中身と同じものである。**
    // **どちらも `cat` の出力で、こちらが定数を持っていないことは変わらない。**
    let position_in_the_file = |prefix: &str| {
        big_lines_after
            .iter()
            .position(|line| line.starts_with(prefix))
    };
    let window_followed_the_cursor = window_rows.len() == 3
        && window_rows[0] != window_rows[1]
        && match (
            position_in_the_file(window_rows[0]),
            position_in_the_file(window_rows[1]),
        ) {
            (Some(before), Some(after)) => after > before,
            _ => false,
        };
    // **上へ戻ると、窓も戻る。** **同じ行が先頭に出ることが主張である**
    // ——**戻り方（先頭にする枝）はホストテストが覆っているが、実機では
    // ここが初めての通過である。**
    let window_came_back_up =
        window_rows.len() == 3 && window_rows[2] == window_rows[0] && !window_rows[0].is_empty();

    // **エラーがエコーエリアに出ている（ADR-0046）。画面の実物で見る。**
    //
    // **台本は `/nope/x` を開いて `:w` を打つ。** **親のディレクトリが無いので
    // 断られる**——**`zi` が代替画面に居る間にエラーを出す唯一の道である。**
    //
    // **主張は 2 つあり、どちらも要る。**
    //
    // **(1) エラーがエコーエリア（最下行）に出ていること。** **溜めるだけで
    // 描かなければ、画面は壊れないが人にも見えない**——**それは却下した
    // (a) と (a') と同じ振る舞いで、区別が付かなくなる。**
    //
    // **(2) 本文の行が壊れていないこと。** **`screen-window` が画面の行 0 を
    // 読んでおり、そちらが持っている。**
    //
    // **期待値をホストが持たない**とは言えない——**文言は `zi` の側にある。**
    // **`zi` の断り書きであることが分かる形で見る**（`zi:` で始まり、
    // `writing` を含む）。**丸ごと写すと、文言を直すたびにここも直す。**
    let echo_line = serial
        .lines()
        .find_map(|line| line.split("screen-echo: ").nth(1))
        .map(|rest| rest.trim().trim_end_matches('\r'))
        .unwrap_or("");
    let error_reached_the_echo_area = echo_line.contains("zi:") && echo_line.contains("writing");

    // **Enter と Backspace と Delete（zi-f）。**
    //
    // **台本が新しいファイルを開き、3 つを通してから保存している**
    // （`kernel/src/input.rs` の台本の doc に打鍵が並べてある）。
    // **読み戻しは `ab` と `c` の 2 行になるはずである。**
    // **区切りは次の `zi` の回の手前である（H-b-2）。**
    //
    // **`script-done` で切っていた。** **台本の末尾に回が 3 つ増えた**ので、
    // **そのままだと後ろの回の出力まで飲み込む**（`/data/lines` の回で
    // 一度踏んだのと同じ形である。**台本を変えたら、台本に寄りかかっている
    // 判定を数え直すこと**）。
    let edited_output = program_output(
        plain
            .split("/bin/cat /data/edited")
            .nth(1)
            .unwrap_or("")
            .split("/bin/zi /data/joined")
            .next()
            .unwrap_or(""),
    );
    let edited_lines_seen: Vec<&str> = edited_output.lines().collect();

    // **(1) Enter が行を割った**——**新しいファイルは 1 行で始まる**ので、
    // **2 行あることが Enter の効いた証拠である。**
    let enter_split_the_line = edited_lines_seen.len() == 2;
    // **(2) Backspace が `X` を消した。**
    let backspace_erased = !edited_output.contains('X');
    // **(3) Delete が `d` を消した。**
    let delete_erased = !edited_output.contains('d');
    // **(4) 3 つが揃った形になっていること。**
    let edited_round_trip = edited_lines_seen == ["ab", "c"];

    // **`a` は `i` と違う桁から挿入する（e-4）。**
    //
    // **台本は `i`（そのまま）と `a`（1 つ右）を両方通す。** `zi` は
    // 判定行の札を分けている（`insert` と `append`）ので、**直前の報告の桁と
    // 比べる**——`i` は同じ桁、`a` は 1 つ右である。
    // **期待値を写していない**（数はどちらも `zi` の報告から取る）。
    let cursor_reports: Vec<(u32, &str)> = serial
        .lines()
        .filter(|line| line.contains("zi: cursor"))
        .filter_map(|line| {
            let col = line.split("col=").nth(1)?;
            let digits: String = col.chars().take_while(char::is_ascii_digit).collect();
            let tag = line.trim_end().rsplit(' ').next()?;
            Some((digits.parse().ok()?, tag))
        })
        .collect();
    let column_before = |index: usize| {
        cursor_reports
            .get(index.wrapping_sub(1))
            .map(|(col, _)| *col)
    };
    let insert_kept_the_column = cursor_reports
        .iter()
        .position(|(_, tag)| *tag == "insert")
        .is_some_and(|at| column_before(at) == Some(cursor_reports[at].0));
    let append_moved_right = cursor_reports
        .iter()
        .position(|(_, tag)| *tag == "append")
        .is_some_and(|at| {
            column_before(at).is_some_and(|before| cursor_reports[at].0 == before + 1)
        });
    let append_differs_from_insert = insert_kept_the_column && append_moved_right;

    // **判定ではない。** **台本が伸びたときに [`ZI_TEST_TIMEOUT`] を決め直す
    // ための実測値である。** **揺れる値なので、合否には載せない。**
    println!(
        "{context}: (info) the script finished {finished_after:?} into the wait \
         (the wait allows {ZI_TEST_TIMEOUT:?}; None means it never finished)"
    );
    // **合図である（2026-09-04）。** **偽なら台本ごと動かないので、
    // 「これだけが落ちる破壊」は在りえない。** **合否には載せたままで、
    // 読み方だけを分ける**——`tools/judgement-map.py` が `(signal)` を数えない。
    println!("{context}: (signal) zi started = {started}");
    println!(
        "{context}: the up/down arrows moved the cursor between lines = {arrows_moved} \
         (rows seen with the arrow tag: {arrow_rows:?})"
    );
    println!(
        "{context}: hjkl reached at least three distinct rows = {hjkl_moved} \
         (rows seen with the move tag: {move_rows:?})"
    );
    println!(
        "{context}: insert mode typed into the buffer = {typed} \
         ({typed_events} typed event(s); the script sends 3)"
    );
    println!("{context}: Esc returned to normal mode = {back_to_normal}");
    println!("{context}: x deleted a byte = {deleted}");
    println!("{context}: :w wrote every byte it asked for = {saved}");
    println!(
        "{context}: cat read back exactly what zi edited = {roundtrip} \
         (the screen said {edited_lines:?}, cat printed {readback:?})"
    );
    println!("{context}: the zash prompt name is drawn in its own color = {prompt_colored}");
    println!("{context}: the prompt symbol kept the default color = {prompt_symbol_plain}");
    println!(
        "{context}: ioctl(TIOCGWINSZ) agrees with the console = {winsize_agrees} \
         (kernel {kernel_geometry:?}, zi {zi_geometry:?})"
    );
    println!("{context}: the zi status line is drawn in its own color = {status_colored}");
    println!("{context}: the zi status line followed the mode = {status_followed_mode}");
    println!(
        "{context}: a lone Esc settled without another key = {esc_settled_at_once} \
         (status labels in order: {status_labels:?})"
    );
    // **窓が 1 行動く移動は、1 行ぶんしか描かないこと（PERF-e）。**
    //
    // **`less` と同じ形の判定である**（あちらは上限 200 字）。**`zi` の実測は
    // 51 字で、全部描き直すと 944 字である**——**上限は `zi` の実測から
    // 決めた**（`less` の値を写さない）。**字の数は揺れない。**
    let zi_moves_one_line = zi_move_cost
        .as_ref()
        .and_then(|values| values.get(3).copied())
        .is_some_and(|glyphs| glyphs <= 200);

    println!(
        "{context}: moving the window one line draws one line = {zi_moves_one_line} \
         (it drew {:?} glyph(s); the whole-screen redraw measured 944)",
        zi_move_cost
            .as_ref()
            .and_then(|values| values.get(3).copied())
    );
    // **空読み 1 回の費用（PERF-f）。** **何も描いていないのに転送が
    // 走っていないかを見る**（`ADR-0047` で、読むたびに掃く形にした）。
    let zi_idle_cost: Option<Vec<u64>> = match (zi_costs.get(5), zi_costs.get(6)) {
        (Some(before), Some(after)) if before.len() == after.len() => Some(
            before
                .iter()
                .zip(after)
                .map(|(x, y)| y.saturating_sub(*x))
                .collect(),
        ),
        _ => None,
    };
    // **空読み 1 回で転送が走らないこと（PERF-f）。**
    //
    // **何も描いていないのだから、送るものも無い。** **`ADR-0047` で
    // 「読むたびに掃く」形にしたとき、カーソルを必ず描き直す経路が
    // そのまま転送になっていた**（実測。**転送 1 回・64 バイト・
    // 49,968 サイクル**）。
    //
    // **アプリは `-EAGAIN` で毎秒 39,000 回ほど回る**ので、
    // **1 回の費用がそのまま CPU の占有になる。**
    //
    // **回数で見る**——**揺れない。** **0 か 1 かである。**
    let idle_read_costs_nothing = zi_idle_cost
        .as_ref()
        .and_then(|values| values.get(7).copied())
        .is_some_and(|flushes| flushes == 0);

    // **1 字の挿入は 1 行ぶんしか描かないこと（PERF-g）。**
    //
    // **実測**——**1 行だけ描くと 232 字、全面だと 1,125 字である。**
    // **上限を 500 字に置くと、両側に 2 倍以上の余裕がある。**
    // **窓の移動の上限（200）を写していない**——**`zi` は状態行と
    // コマンド行の 2 本を毎回描くので、下限がそのぶん高い。**
    let insert_draws_one_line = zi_insert_cost
        .as_ref()
        .and_then(|values| values.get(3).copied())
        .is_some_and(|glyphs| glyphs <= 500);

    println!(
        "{context}: inserting one character draws one line = {insert_draws_one_line} \
         (it drew {:?} glyph(s); the whole-screen redraw measured 1125)",
        zi_insert_cost
            .as_ref()
            .and_then(|values| values.get(3).copied())
    );
    println!(
        "{context}: (info) inserting one character costs {zi_insert_cost:?} \
         [syscalls writes write_bytes glyphs draw_cycles erase_cycles glyph_cycles flushes \
         flush_bytes flush_cycles full_flushes ticks]"
    );
    println!(
        "{context}: an idle read sends nothing = {idle_read_costs_nothing} \
         (it caused {:?} transfer(s); before PERF-f it was 1 transfer of 64 bytes)",
        zi_idle_cost
            .as_ref()
            .and_then(|values| values.get(7).copied())
    );
    println!(
        "{context}: (info) one idle read (no output at all) costs {zi_idle_cost:?} \
         [syscalls writes write_bytes glyphs draw_cycles erase_cycles glyph_cycles flushes \
         flush_bytes flush_cycles full_flushes ticks]"
    );
    println!(
        "{context}: (info) 60 j keystrokes cost {zi_run_cost:?} \
         [syscalls writes write_bytes glyphs draw_cycles erase_cycles glyph_cycles flushes \
         flush_bytes flush_cycles full_flushes ticks] - the last field is 10 ms per tick"
    );
    println!(
        "{context}: (info) the cost of one j that moves the window = {zi_move_cost:?} \
         [syscalls writes write_bytes glyphs draw_cycles erase_cycles glyph_cycles flushes \
         flush_bytes flush_cycles full_flushes ticks]"
    );
    println!("{context}: the screen before the alternate screen came back = {screen_came_back}");
    println!(
        "{context}: the cursor on the screen followed the buffer = {cursor_followed} \
         (screen {cursor_cell:?}, buffer says {buffer_cursor:?} as (row - top, col))"
    );
    println!("{context}: the zi status line sits on the second-to-last row = {status_at_bottom}");
    println!(
        "{context}: the command line echoes what is being typed = {command_line_echoes} \
         ({command_line:?})"
    );
    println!(
        "{context}: the refusal is shown on the command line = {message_shown} ({message_line:?})"
    );
    println!(
        "{context}: a new file was created and read back = {} \
         (it appeared in ls = {created_file_appeared}, cat printed what zi wrote = {fresh_content})",
        created_file_appeared && fresh_content
    );
    println!("{context}: rm removed it again = {removed_file_disappeared}");
    println!(
        "{context}: mkdir made a directory and touch put a file in it = \
         {made_directory_holds_the_file} (ls /tmp/box printed {listing_in_box:?})"
    );
    println!("{context}: rmdir refused a directory that was not empty = {rmdir_refused}");
    println!(
        "{context}: the directory was gone after the round trip = {directory_removed} \
         (ls /tmp printed {listing_of_tmp:?})"
    );
    println!(
        "{context}: brk gave back every frame it took = {brk_returned_what_it_took} \
         (took/gave {heap_numbers:?}, from {heap_line:?})"
    );
    println!(
        "{context}: zi gave back every frame it took, on every run = \
         {zi_returned_what_it_took} (took/gave per run {zi_heap_pairs:?})"
    );
    println!(
        "{context}: one brk grew past two pages = {brk_grew_past_two_pages} \
         (largest take across zi runs: {zi_largest_take} page(s))"
    );
    println!(
        "{context}: backspace at the start of a line joined it to the one above = \
         {backspace_joined_the_lines} (cat printed {joined_lines_seen:?})"
    );
    println!(
        "{context}: zi opened a file past both of the old limits = \
         {} (lines {} > 64 = {big_passes_the_old_line_limit}, longest line {} > 128 = \
         {big_passes_the_old_length_limit})",
        big_passes_the_old_line_limit && big_passes_the_old_length_limit,
        big_lines_before.len(),
        big_lines_before
            .iter()
            .map(|line| line.len())
            .max()
            .unwrap_or(0)
    );
    println!(
        "{context}: the big file read back with exactly the one edit = {big_round_trip} \
         (before {} line(s), after {} line(s))",
        big_lines_before.len(),
        big_lines_after.len()
    );
    println!(
        "{context}: the window followed the cursor down the file = \
         {window_followed_the_cursor} (the top row of the window said {window_rows:?}, \
         at lines {:?} of what cat printed)",
        window_rows
            .iter()
            .map(|row| position_in_the_file(row))
            .collect::<Vec<_>>()
    );
    println!(
        "{context}: the window went back up to where it started = {window_came_back_up} \
         (the three window rows are {window_rows:?})"
    );
    println!(
        "{context}: the error reached the echo area instead of the text = \
         {error_reached_the_echo_area} (the last row said {echo_line:?})"
    );
    println!(
        "{context}: enter split the line = {enter_split_the_line}, backspace erased = \
         {backspace_erased}, delete erased = {delete_erased}, the round trip reads back as \
         written = {edited_round_trip} (cat printed {edited_lines_seen:?})"
    );
    println!("{context}: the save reached the device = {save_reached_the_device}");
    println!(
        "{context}: leaving the alternate screen skips the blank cells = \
         {repaint_skips_blank_cells} (it drew {repaint_cells:?} cell(s) over {repaints:?} \
         repaint(s); the screen holds {screen_size:?})"
    );
    println!(
        "{context}: the only failure in the round was the refused rmdir = \
         {only_the_refused_rmdir_failed} ({failures_in_the_round} non-zero exit(s))"
    );
    println!(
        "{context}: tail printed the end of the file and nothing more = \
         {tail_matches_the_end_of_cat} (tail {tail_output:?}, cat {cat_output:?})"
    );
    println!(
        "{context}: a starts one column right of i = {append_differs_from_insert} \
         (i kept the column = {insert_kept_the_column}, a moved right = {append_moved_right})"
    );
    println!(
        "{context}: note - the continuation-cell flag (Cell::CONTINUATION) has no screen-side \
         judgement here; the font has no two-cell glyph, so a continuation cell never appears \
         on the real screen. the claim lives in the host test a_wide_glyph_marks_its_right_half"
    );
    for line in serial
        .lines()
        .filter(|line| line.contains("screen-restore:"))
    {
        println!("  {}", line.trim());
    }
    for line in serial.lines().filter(|line| line.contains("screen-color:")) {
        println!("  {}", line.trim());
    }
    println!(
        "{context}: note - the judgements above the screen-color ones read zi's internal state, \
         NOT the screen; a redraw that puts the right bytes at the wrong place is still not \
         observable here. the screen-color judgements (ES-d) read the back buffer, but only \
         at the colored cells - they say nothing about where the text landed"
    );

    if started
        && arrows_moved
        && hjkl_moved
        && typed
        && back_to_normal
        && deleted
        && saved
        && roundtrip
        && save_reached_the_device
        && repaint_skips_blank_cells
        && prompt_colored
        && prompt_symbol_plain
        && winsize_agrees
        && status_colored
        && status_followed_mode
        && esc_settled_at_once
        && screen_came_back
        && cursor_followed
        && zi_moves_one_line
        && insert_draws_one_line
        && idle_read_costs_nothing
        && status_at_bottom
        && command_line_echoes
        && message_shown
        && created_file_appeared
        && fresh_content
        && removed_file_disappeared
        && tail_matches_the_end_of_cat
        && made_directory_holds_the_file
        && rmdir_refused
        && directory_removed
        && only_the_refused_rmdir_failed
        && enter_split_the_line
        && backspace_erased
        && delete_erased
        && edited_round_trip
        && brk_returned_what_it_took
        && zi_returned_what_it_took
        && brk_grew_past_two_pages
        && backspace_joined_the_lines
        && big_passes_the_old_line_limit
        && big_passes_the_old_length_limit
        && big_round_trip
        && window_followed_the_cursor
        && window_came_back_up
        && error_reached_the_echo_area
        && append_differs_from_insert
    {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAILED")
    }
}

/// シリアルの一区間から、プログラムが出した行だけを取り出す（zi-d-2）。
///
/// **シリアルには判定行とログが混ざる**ので、`[` で始まる行とプロンプトを
/// 落とす。**空行も落とす**——`/data` の検査用のファイルに空行は無い。
///
/// **`zi-test` と `view-test` が同じものを使う**（VIEW-b で閉包から切り出した。
/// **同じ切り出し方を2つ書かない**）。
fn program_output(segment: &str) -> String {
    segment
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.starts_with('[') && !line.contains("zaytos$") && !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `less` の実演（VIEW-b）。**決定的な台本入力で駆動する。**
///
/// # `zi-test` と分けてある
///
/// **`zi-test` は21秒掛かり、破壊19構成すべてに掛かる**（実測）。
/// **`less` の打鍵をあちらへ足すと、19回ぶん伸びる**（運用者の承認。VIEW-b の 4-4）。
///
/// # 判定は画面の実物である
///
/// **`less` は内部状態を1つも出さない**（`zi` の判定行に当たるものが無い）。
/// **主張は「窓の外に在った行が見えるようになったこと」と
/// 「抜けたら元の画面へ戻ること」で、どちらも画面を読む。**
///
/// **期待値をホストが持たない**——**`cat` の出した並びの中で、画面に出た行が
/// どこに在るかで見る。** **像の中身も、窓の高さも、動いた量も写していない。**
fn cmd_view_test(features: &[&str]) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let mut all_features: Vec<&str> = vec!["view-test"];
    all_features.extend_from_slice(features);
    let kernel_elf = build_kernel_with_features(&workspace_root, &all_features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = all_features.join("-");
    let serial_log = workspace_root
        .join("target")
        .join(format!("view-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the view test")?;

    // **合図は `script-done:` である**（`zi-test` と同じ。**主張を持つ行を
    // 待ちの合図に使わない**）。
    let done_marker = "script-done:";
    let started_waiting = Instant::now();
    let deadline = started_waiting + ZI_TEST_TIMEOUT;
    let mut finished_after = None;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if strip_ansi(&text).contains(done_marker) {
            finished_after = Some(started_waiting.elapsed());
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let context = if features.is_empty() {
        "view-test".to_string()
    } else {
        format!("view-test {}", features.join("+"))
    };
    let context = context.as_str();

    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    let plain = strip_ansi(&serial);

    // **`cat` の出した並びを 2 回撮る。** **前は `less` の手前、後は末尾である。**
    // **期待値をホストが持たない**——**画面に出た行がこの並びのどこに在るかで見る。**
    let cat_before = program_output(
        plain
            .split("/bin/cat /data/big")
            .nth(1)
            .unwrap_or("")
            .split("/bin/less /data/big")
            .next()
            .unwrap_or(""),
    );
    // **区切りは次の回の手前である（VIEW-c で台本が伸びた）。**
    // **`script-done` で切っていたが、後ろに `more` の 2 回と `cat` が付いた**
    // ——**台本を変えたら、台本に寄りかかっている判定を数え直すこと。**
    let cat_after = program_output(
        plain
            .split("/bin/cat /data/big")
            .nth(2)
            .unwrap_or("")
            .split("/bin/more /data/big")
            .next()
            .unwrap_or(""),
    );
    // **`more` の後にもう一度撮る**——**`more` も像を変えていないこと。**
    let cat_final = program_output(
        plain
            .split("/bin/cat /data/big")
            .nth(3)
            .unwrap_or("")
            .split("script-done")
            .next()
            .unwrap_or(""),
    );
    let lines_before: Vec<&str> = cat_before.lines().collect();
    let lines_after: Vec<&str> = cat_after.lines().collect();
    let lines_final: Vec<&str> = cat_final.lines().collect();

    // **窓の観測**（`kernel/src/console/probe.rs` の `observe_view_window`）。
    // **3 回ある**——入った直後、`Space` の後、戻した後である。
    let views: Vec<(String, String)> = serial
        .lines()
        .filter_map(|line| line.split("screen-view: the top row says ").nth(1))
        .filter_map(|rest| {
            let (top, tail) = rest.split_once(" and the last text row ")?;
            let bottom = tail.split_once("says ").map(|(_, value)| value)?;
            Some((
                top.trim().trim_matches('"').to_string(),
                bottom
                    .trim()
                    .trim_end_matches('\r')
                    .trim_matches('"')
                    .to_string(),
            ))
        })
        .collect();
    let position = |prefix: &str| -> Option<usize> {
        if prefix.is_empty() {
            return None;
        }
        lines_before
            .iter()
            .position(|line| line.starts_with(prefix))
    };

    // **(1) 開いた直後、ファイルの先頭が見えている。**
    let opened_at_the_top = views.len() == 3 && position(&views[0].0) == Some(0);
    // **(2) 窓の外に在った行が見えるようになった。**
    //
    // **`Space` の後の上端が、動かす前の下端より後ろに在ること。**
    // **これが「窓の外」の意味である**——**動かす前の画面には出ていなかった。**
    let saw_past_the_first_screen = views.len() == 3
        && match (position(&views[0].1), position(&views[1].0)) {
            (Some(was_bottom), Some(now_top)) => now_top > was_bottom,
            _ => false,
        };
    // **(3) 戻したら同じところへ戻る。** **`j` と `k` が釣り合い、`b` が
    // `Space` を打ち消す。**
    let came_back_to_the_top =
        views.len() == 3 && views[2].0 == views[0].0 && !views[0].0.is_empty();
    // **(4) 抜けたら元の画面が戻った。** **カーネル側の判定行をそのまま読む。**
    let screen_restore_line = serial
        .lines()
        .find_map(|line| line.split("screen-restore: the screen ").nth(1))
        .map(|rest| rest.trim().trim_end_matches('\r').to_string())
        .unwrap_or_default();
    let screen_came_back = screen_restore_line.contains("came back = true");
    // **(5) `less` は像を変えていない。** **前後の `cat` が一致すること。**
    let image_unchanged =
        !lines_before.is_empty() && lines_before == lines_after && lines_before == lines_final;

    // **(6) `more` は抜けても出力が残る（VIEW-c）。`less` と逆の主張である。**
    //
    // **`less` の判定を流用しない**——**あちらは「元の画面が戻ったこと」で、
    // 主張が逆である。** **こちらは「出したものが画面に在ること」を見る。**
    //
    // **期待値をホストが持たない**——**画面の下 3 行のどれかが、`cat` の出した
    // 並びの中に在ることを見る。** **どの行かは書かない**（**画面の高さと
    // ファイルの長さの関係を写すことになる**）。
    let more_rows: Vec<String> = serial
        .lines()
        .filter_map(|line| line.split("screen-more: row ").nth(1))
        .filter_map(|rest| rest.split_once("says ").map(|(_, value)| value))
        .map(|value| {
            value
                .trim()
                .trim_end_matches('\r')
                .trim_matches('"')
                .to_string()
        })
        .collect();
    let more_output_stayed = !more_rows.is_empty()
        && more_rows
            .iter()
            .any(|row| !row.is_empty() && position(row).is_some());

    // **描画の費用（PERF）。** **主張しない**——**層ごとの数を差で出すだけである。**
    // **どの層が重いかを、推測ではなく実測で見るために在る。**
    let costs: Vec<Vec<u64>> = serial
        .lines()
        .filter_map(|line| line.split("screen-cost: ").nth(1))
        .map(|rest| {
            rest.split_whitespace()
                .filter_map(|field| field.split('=').nth(1))
                .filter_map(|value| value.trim_end_matches('\r').parse::<u64>().ok())
                .collect()
        })
        .collect();
    let delta = |from: usize, to: usize| -> Option<Vec<u64>> {
        let (a, b) = (costs.get(from)?, costs.get(to)?);
        if a.len() != b.len() {
            return None;
        }
        Some(a.iter().zip(b).map(|(x, y)| y.saturating_sub(*x)).collect())
    };
    let cost_fields = "syscalls writes write_bytes glyphs draw_cycles erase_cycles glyph_cycles \
         flushes flush_bytes flush_cycles full_flushes ticks";

    // **1 回の動きにつき、転送は 1 回である（ADR-0047）。**
    //
    // **回数を判定に載せ、サイクルと量は (info) に置く**——**回数は揺れないが、
    // サイクルは揺れる**（実測。同じ構成で 4.5M と 4.7M）。
    //
    // **`ADR-0047` の前は 152 回だった**（実測）。**破壊 `flush-every-write` が
    // その形へ戻す。**
    // **1 行の移動は 1 行ぶんしか描かないこと（PERF-d）。**
    //
    // **窓が 1 行動くと、本文の行はすべて別の行を映す**ので、
    // **「変わった行だけ描く」では 1 字も減らない**——**画面をずらすことで
    // 初めて減る**（`ADR-0040` の Addendum）。
    //
    // **実測**——**ずらすと 55 字、全部描き直すと 967 字である。**
    // **上限を 200 字に置くと、両側に 3 倍以上の余裕がある。**
    // **字の数は揺れない**（同じ台本なら同じである）。
    let one_line_redraws_one_line = delta(2, 3)
        .and_then(|values| values.get(3).copied())
        .is_some_and(|glyphs| glyphs <= 200);

    // **消す費用が、同じ量を送る費用と同じ桁であること（PERF-c）。**
    //
    // # なぜ比で見るのか
    //
    // **サイクルは揺れ、機械の周波数にも依る**（TSC はホストのサイクルである）。
    // **同じ回の転送と比べれば、その両方が消える**——**どちらも 1 画面ぶん
    // （4MB 前後）を動かす仕事である。**
    //
    // **実測**——**一括で消すと転送の 9.8 倍、1 画素ずつ消すと 113 倍である。**
    // **上限を 30 倍に置くと、両側に 3 倍ほどの余裕がある。**
    //
    // **`ADR-0047` と `PERF-b` の判定が回数で言えるのと違い、ここは量の話なので
    // 回数では言えない**——**同じ画素数を、遅い道と速い道のどちらで書いたかである。**
    let erase_is_bulk = match delta(0, 1) {
        Some(values) => match (values.get(5), values.get(9)) {
            (Some(&erase), Some(&flush)) if flush > 0 => erase <= flush.saturating_mul(30),
            _ => false,
        },
        None => false,
    };

    // **1 回の動きにつき、システムコールも 1 回である（PERF-b）。**
    //
    // **`ADR-0047` の前は 152 回だった**（実測。**1 行につき 3 回**）。
    // **`write` のたびに BKL を解いて取り直しているので、回数がそのまま費用である。**
    //
    // **転送の判定とは別に持つ**——**片方だけが落ちる形が在る**
    // （破壊 `frame-write-per-piece` は回数だけを戻し、転送は 1 回のままである）。
    let one_syscall_per_move = matches!(
        (
            delta(0, 1).and_then(|values| values.first().copied()),
            delta(2, 3).and_then(|values| values.first().copied()),
        ),
        (Some(1), Some(1))
    );
    let one_transfer_per_move = matches!(
        (
            delta(0, 1).and_then(|values| values.get(7).copied()),
            delta(2, 3).and_then(|values| values.get(7).copied()),
        ),
        (Some(1), Some(1))
    );

    // **合図である（2026-09-04）。** 上の `zi started` と同じ。
    println!(
        "{context}: (signal) script finished = {} ({:?})",
        finished_after.is_some(),
        finished_after
    );
    println!(
        "{context}: (info) the cost of one Space (a whole page) = {:?} [{cost_fields}]",
        delta(0, 1)
    );
    println!(
        "{context}: (info) the cost of one j (a single line) = {:?} [{cost_fields}]",
        delta(2, 3)
    );
    println!(
        "{context}: (info) the cost of one Space in more (one page of plain output) = {:?} \
         [{cost_fields}]",
        delta(4, 5)
    );
    println!(
        "{context}: moving one line draws one line = {one_line_redraws_one_line} \
         (j drew {:?} glyph(s); the whole-screen redraw measured 967)",
        delta(2, 3).and_then(|values| values.get(3).copied())
    );
    println!(
        "{context}: erasing a page stays in the same order as sending one = {erase_is_bulk} \
         (erase {:?} cycles, transfer {:?} cycles for the same page; the bulk path measured 9.8x \
         and the pixel-by-pixel path 113x)",
        delta(0, 1).and_then(|values| values.get(5).copied()),
        delta(0, 1).and_then(|values| values.get(9).copied())
    );
    println!(
        "{context}: one move costs one syscall = {one_syscall_per_move} \
         (Space {:?} syscall(s), j {:?} syscall(s); PERF-b replaced one write per piece)",
        delta(0, 1).and_then(|values| values.first().copied()),
        delta(2, 3).and_then(|values| values.first().copied())
    );
    println!(
        "{context}: one move costs one transfer = {one_transfer_per_move} \
         (Space {:?} transfer(s), j {:?} transfer(s); ADR-0047 replaced one transfer per write)",
        delta(0, 1).and_then(|values| values.get(7).copied()),
        delta(2, 3).and_then(|values| values.get(7).copied())
    );
    println!(
        "{context}: less opened at the top of the file = {opened_at_the_top} \
         (the top row said {:?})",
        views.first().map(|view| view.0.as_str())
    );
    println!(
        "{context}: a line past the first screen became visible = {saw_past_the_first_screen} \
         (before: top {:?} bottom {:?}; after Space: top {:?}; at lines {:?} and {:?} of what cat \
         printed)",
        views.first().map(|view| view.0.as_str()),
        views.first().map(|view| view.1.as_str()),
        views.get(1).map(|view| view.0.as_str()),
        views.first().and_then(|view| position(&view.1)),
        views.get(1).and_then(|view| position(&view.0)),
    );
    println!(
        "{context}: moving back up returned to the same line = {came_back_to_the_top} \
         (the three top rows are {:?})",
        views
            .iter()
            .map(|view| view.0.as_str())
            .collect::<Vec<&str>>()
    );
    println!(
        "{context}: the screen came back after less left = {screen_came_back} \
         ({screen_restore_line:?})"
    );
    println!(
        "{context}: neither less nor more changed the file = {image_unchanged} \
         ({} line(s) before, {} after less, {} after more)",
        lines_before.len(),
        lines_after.len(),
        lines_final.len()
    );
    println!(
        "{context}: what more printed is still on the screen after it left = \
         {more_output_stayed} (the last rows say {more_rows:?}, at lines {:?} of what cat printed)",
        more_rows
            .iter()
            .map(|row| position(row))
            .collect::<Vec<Option<usize>>>()
    );

    if finished_after.is_some()
        && opened_at_the_top
        && saw_past_the_first_screen
        && came_back_to_the_top
        && screen_came_back
        && image_unchanged
        && more_output_stayed
        && one_transfer_per_move
        && one_syscall_per_move
        && erase_is_bulk
        && one_line_redraws_one_line
    {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAILED")
    }
}

/// シリアルのログから ANSI のエスケープ列を落とす（ES-d）。
///
/// # なぜ要るのか
///
/// **ES-d でプロンプトに色が付いた**ので、`write` が出すバイト列は
/// `\x1b[38;2;200;0;200mzaytos$ \x1b[0m` である。**シリアルはそれをそのまま
/// 記録する**（`sys_write` はバイトを写すだけである）ので、
/// **`"zaytos$ /bin/ls"` のような「プロンプトの直後に打った語が続く」
/// 目印が、色の列に割られて当たらなくなる。**
///
/// # 隠したものを誰が見ているか
///
/// **この関数は色の列を判定から隠す**（揺れる値ではないが、標識で消す点は
/// 同じ形である）。**隠したものを見ているのは 2 つある**——
/// **起動ログの参照**（`xtask/reference/boot-log-smp2.txt` は素のバイトを
/// 持っており、色が消えれば差分が出る）と、**画面の観測**
/// （`screen-color:` の判定行が、色が実際にセルとピクセルへ届いたことを見る）。
///
/// **落とすのは CSI（`ESC [` … 終端）と単独の `ESC` だけである。**
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // **`[` が続けば CSI である。** 終端バイト（0x40..=0x7E）まで捨てる。
        let mut lookahead = chars.clone();
        if lookahead.next() == Some('[') {
            chars = lookahead;
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        // **`[` が続かなければ、落とすのは `ESC` 1 バイトだけである。**
    }
    out
}

/// `cat` が読み戻した行を拾う（zi-d-2）。
///
/// **`cat` はファイルの中身をそのまま出す**ので、シェルが反響した
/// コマンド行の後ろに、行がそのまま並ぶ。**求める本数だけ取る。**
fn parse_cat_readback(serial: &str, want: usize) -> Vec<String> {
    // **色の列を落としてから探す（ES-d）。** プロンプトに色が付いたので、
    // **素のログでは目印がエスケープに割られる**（[`strip_ansi`] の doc）。
    let serial = strip_ansi(serial);
    let serial = serial.as_str();
    // **プロンプトを目印にしない（DIR-1b で踏んだ）。**
    //
    // **観測の出力がプロンプトと反響の間へ割り込む**——`\x05` を `zi` を
    // 抜けた直後へ移したところ、`screen-restore:` の行が `zaytos$ ` と
    // `/bin/cat ...` の間に入り、**この目印が一致しなくなった**（実測）。
    //
    // **同じ罠は既に 1 つ先で書いてあった**（`cmd_zi_test` の `ls` の一覧の
    // 取り出し）。**そちらは避けていて、こちらは踏んでいた。**
    let marker = "/bin/cat /data/lines";
    let Some(at) = serial.rfind(marker) else {
        return Vec::new();
    };
    // **カーネルのログ行を除く。** `cat` を起こす際の `spawn` と `user-load`
    // の行が同じシリアルへ混ざるので、**`[INFO]` などで始まる行は飛ばす**
    // （`cat` が出すのはファイルの中身だけで、目印を持たない）。
    serial[at + marker.len()..]
        .lines()
        .skip(1)
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.starts_with('[') && !line.is_empty())
        .take(want)
        .map(str::to_string)
        .collect()
}

/// `zi` の判定行から `row=` の値を順に拾う（zi-d）。
///
/// **札で絞る（ES-d の手当て）。** `zi` は動かした理由を行末の札で分けており
/// （`arrow` / `move` / `typed` / `delete` / `normal` / `start` / `command`）、
/// **絞らずに全部拾うと、矢印の主張を `hjkl` が満たしてしまう。**
/// **実測でそうなっていた**——`zi-cursor-ignore-updown` を有効にしても
/// `arrows_moved` が真のままだった（`docs/verification-coverage.md`）。
fn parse_zi_cursor_rows(serial: &str, tag: &str) -> Vec<u32> {
    serial
        .lines()
        .filter(|line| line.contains("zi: cursor") && line.trim_end().ends_with(tag))
        .filter_map(|line| {
            let rest = line.split("row=").nth(1)?;
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .collect()
}

fn cmd_pci_test(features: &[&str]) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    // **構成ごとに別のログへ書く**（`cmd_fs_image_extract` と同じ理由）。
    let tag = if features.is_empty() {
        "default".to_string()
    } else {
        features.join("-")
    };
    let serial_log = workspace_root
        .join("target")
        .join(format!("pci-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);
    let monitor_socket =
        PathBuf::from(format!("/tmp/zaytos-xtask-pci-{}.sock", std::process::id()));
    let _ = fs::remove_file(&monitor_socket);
    ensure_socket_path_fits(&monitor_socket)?;

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: Some(&monitor_socket),
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the pci test")?;

    // 列挙の完了を待つ。上限つき。
    let complete_marker = "pci: enumeration complete:";
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if text.contains(complete_marker) {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let context = if features.is_empty() {
        "pci-test".to_string()
    } else {
        format!("pci-test {}", features.join("+"))
    };
    let context = context.as_str();

    // **同じ起動の QEMU から装置の帳簿を取る。** ここが外の道具である。
    let info_pci = match connect_monitor_with_retry(&monitor_socket) {
        Ok(mut stream) => query_monitor(&mut stream, "info pci").unwrap_or_default(),
        Err(e) => {
            println!("{context}: could not reach the QEMU monitor: {e}");
            String::new()
        }
    };

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&monitor_socket);

    let serial = read_lossy(&serial_log);
    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }
    if !serial.contains(complete_marker) {
        bail!("{context}: the kernel never reported {complete_marker:?}");
    }

    let mut kernel_functions = parse_kernel_pci_lines(&serial);
    kernel_functions.sort();
    let mut qemu_functions = parse_info_pci(&info_pci);
    qemu_functions.sort();

    // **空の帳簿を一致にしない。** モニタが読めなかったときは両方空でも落とす
    // ——「適用外」を緑にしない。
    let sets_match = kernel_functions == qemu_functions && !qemu_functions.is_empty();
    println!(
        "{context}: kernel enumerated {} function(s), qemu reports {}; the sets match = \
         {sets_match} (bus/device/function and vendor:device, both sides read independently)",
        kernel_functions.len(),
        qemu_functions.len()
    );

    let kernel_virtio = count_virtio_blk(&kernel_functions);
    let qemu_virtio = count_virtio_blk(&qemu_functions);
    println!(
        "{context}: virtio-blk (1af4:1001 or 1af4:1041) in the kernel's list = {kernel_virtio}, \
         in qemu's list = {qemu_virtio} (wanted exactly 1 in both)"
    );

    // **ポートで読むという前提の観測。** 0 でなくなったら設計の見直しである。
    let mcfg_zero = serial.contains("acpi: MCFG tables: 0 ");
    println!(
        "{context}: the kernel observed 0 MCFG tables = {mcfg_zero} (the premise of the \
         port 0xCF8/0xCFC access)"
    );

    if !sets_match || kernel_virtio != 1 || qemu_virtio != 1 || !mcfg_zero {
        bail!("{context}: the pci enumeration does not agree with qemu's own device list");
    }
    Ok(())
}

/// カーネルの列挙の判定行から `(bus, device, function, "vvvv:dddd")` を拾う。
///
/// **形の合わない行は黙って読み飛ばさず、そもそも拾えない**——各段の
/// 突き合わせ（`device` / `function` / コロン）に外れた時点で次の行へ進む。
/// `pci: enumeration complete:` のような同じ接頭辞の行はここで弾かれる。
fn parse_kernel_pci_lines(serial: &str) -> Vec<(u8, u8, u8, String)> {
    let mut out = Vec::new();
    for line in serial.lines() {
        let Some((_, rest)) = line.split_once("pci: bus ") else {
            continue;
        };
        let mut tokens = rest.split_whitespace();
        let Some(bus) = tokens.next().and_then(|t| t.parse::<u8>().ok()) else {
            continue;
        };
        if tokens.next() != Some("device") {
            continue;
        }
        let Some(device) = tokens.next().and_then(|t| t.parse::<u8>().ok()) else {
            continue;
        };
        if tokens.next() != Some("function") {
            continue;
        }
        let Some(function) = tokens
            .next()
            .and_then(|t| t.strip_suffix(':'))
            .and_then(|t| t.parse::<u8>().ok())
        else {
            continue;
        };
        let Some(id) = tokens.next() else {
            continue;
        };
        out.push((bus, device, function, id.to_string()));
    }
    out
}

/// `info pci` の出力から `(bus, device, function, "vvvv:dddd")` を拾う。
///
/// **`PCI subsystem` の行は拾わない**——装置の ID の行は `: PCI device ` を
/// 含み、subsystem の行は含まない。
fn parse_info_pci(text: &str) -> Vec<(u8, u8, u8, String)> {
    let mut out = Vec::new();
    let mut current: Option<(u8, u8, u8)> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("Bus") {
            // 「Bus  0, device   4, function 0:」——数字だけを順に拾う。
            let mut numbers = line
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse::<u8>().ok());
            current = match (numbers.next(), numbers.next(), numbers.next()) {
                (Some(bus), Some(device), Some(function)) => Some((bus, device, function)),
                _ => None,
            };
        } else if let Some((_, id_part)) = line.split_once(": PCI device ") {
            if let (Some((bus, device, function)), Some(id)) =
                (current, id_part.split_whitespace().next())
            {
                out.push((bus, device, function, id.to_string()));
            }
        }
    }
    out
}

/// virtio-blk の数を数える。transitional（`1001`）と modern（`1041`）の両方。
fn count_virtio_blk(functions: &[(u8, u8, u8, String)]) -> usize {
    functions
        .iter()
        .filter(|(_, _, _, id)| id == "1af4:1001" || id == "1af4:1041")
        .count()
}

/// モニタへ 1 コマンドを送り、次のプロンプトまでの応答を返す。
///
/// **どちらの待ちも上限つきである**（`CLAUDE.md` のシェルコマンドの制約と
/// 同じ理由。パイプの相手が死んでいる状況は普通に起きる）。
fn query_monitor(stream: &mut UnixStream, command: &str) -> Result<String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .context("failed to set the monitor read timeout")?;
    let mut scratch = [0u8; 4096];
    // 接続直後のバナーと最初のプロンプトを読み捨てる。
    let mut banner = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !banner.contains("(qemu)") {
        if Instant::now() >= deadline {
            bail!("timed out waiting for the monitor banner");
        }
        match stream.read(&mut scratch) {
            Ok(0) => bail!("the monitor closed the connection"),
            Ok(n) => banner.push_str(&String::from_utf8_lossy(&scratch[..n])),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e).context("failed to read the monitor banner"),
        }
    }
    stream
        .write_all(format!("{command}\n").as_bytes())
        .context("failed to send the monitor command")?;
    let mut response = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !response.contains("(qemu)") {
        if Instant::now() >= deadline {
            bail!("timed out waiting for the monitor response");
        }
        match stream.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => response.push_str(&String::from_utf8_lossy(&scratch[..n])),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e).context("failed to read the monitor response"),
        }
    }
    Ok(response)
}

/// `info blockstats` の出力から `disk0` の `rd_bytes` を拾う（S13-c）。
fn parse_disk0_rd_bytes(text: &str) -> Option<u64> {
    parse_disk0_stat(text, "rd_bytes=")
}

/// `info blockstats` の出力から `disk0` の `wr_bytes` を拾う（S13-e。rd の対称）。
fn parse_disk0_wr_bytes(text: &str) -> Option<u64> {
    parse_disk0_stat(text, "wr_bytes=")
}

/// `disk0:` の行から `marker` 直後の数を拾う。
fn parse_disk0_stat(text: &str, marker: &str) -> Option<u64> {
    for raw in text.lines() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("disk0:") else {
            continue;
        };
        let Some((_, after)) = rest.split_once(marker) else {
            continue;
        };
        return after.split_whitespace().next()?.parse().ok();
    }
    None
}

/// 像の検査値（P-e。`ADR-0034` の Addendum）。
///
/// **カーネル側の `image_checksum` と同じ式である**——`byte * (index + 1)` の
/// 総和をラップさせて足す。**式を 2 つに増やさない。**
///
/// **源は独立である**——**こちらはファイルを直に読み、あちらは virtio を通って
/// 読んだ複製を見ている。**
fn image_checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for (index, byte) in bytes.iter().enumerate() {
        sum = sum.wrapping_add(u32::from(*byte).wrapping_mul(index as u32 + 1));
    }
    sum
}

/// 像のロードの破壊の一覧（S13-c）。
const FS_LOAD_SABOTAGES: &[(&str, &str)] = &[
    // **`fs-load-from-embedded-test` は P-e で消した。** **埋め込み像を外したので、
    // 装置以外の源が無い**——**戻す先が無い**（`ADR-0034` の Addendum）。
    ("a skipped first chunk", "virtio-load-skip-first-test"),
];

/// virtio-blk の読みの破壊の一覧（S13-b）。
const VIRTIO_SABOTAGES: &[(&str, &str)] = &[
    ("a dropped queue notify", "virtio-skip-notify-test"),
    ("a request for the wrong sector", "virtio-wrong-sector-test"),
    ("a data descriptor one byte short", "virtio-short-desc-test"),
];

/// カーネルが読んだ sector 0 を、ホスト側の像のファイルと突き合わせる（S13-b）。
///
/// **判定の出所は `target/disk0.img` そのものである。** `stage_esp` が模様を
/// 置いて建て、カーネルは装置越しに読んで checksum を出し、こちらは同じ
/// ファイルの同じ 512 バイトから同じ計算をする。**両側が独立である。**
fn cmd_virtio_test(features: &[&str]) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = if features.is_empty() {
        "default".to_string()
    } else {
        features.join("-")
    };
    let serial_log = workspace_root
        .join("target")
        .join(format!("virtio-test-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the virtio test")?;

    // 読みの判定行か、停止の行が出るまで待つ。上限つき。
    let read_marker = "virtio-blk: read sector ";
    let error_marker = "virtio-blk: ";
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if text.contains(read_marker)
            || text
                .lines()
                .any(|l| l.contains("[ERROR]") && l.contains(error_marker))
        {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let context = if features.is_empty() {
        "virtio-test".to_string()
    } else {
        format!("virtio-test {}", features.join("+"))
    };
    let context = context.as_str();

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    let Some(read_line) = serial.lines().find(|l| l.contains(read_marker)) else {
        // 停止していれば、その行を見せて落とす（skip-notify はここへ来る）。
        if let Some(line) = serial
            .lines()
            .find(|l| l.contains("[ERROR]") && l.contains(error_marker))
        {
            bail!("{context}: the read did not complete: {}", line.trim());
        }
        bail!("{context}: the kernel reported neither the read line nor an error");
    };
    println!("{context}: {}", read_line.trim());

    // **ホスト側で同じ計算をする。** 出所は像のファイルそのものである。
    // **読むのは sector 2（オフセット 1024。superblock）である**——S13-c で
    // ディスクの中身が ext2 の像になり、sector 0 は boot 領域の全 0 になった。
    // 0 のままでは「読めていなくても 0」で一致が言えない（族の 1 つ目）。
    let image = fs::read(disk_image_path(&esp_dir))
        .with_context(|| "failed to read the disk image back".to_string())?;
    let mut expected_checksum = 0u32;
    for (index, byte) in image.iter().skip(1024).take(512).enumerate() {
        expected_checksum =
            expected_checksum.wrapping_add(u32::from(*byte).wrapping_mul(index as u32 + 1));
    }
    let kernel_checksum = parse_marked_hex(read_line, "checksum=");
    let checksum_matches = kernel_checksum == Some(expected_checksum);
    // **`{:#x?}` を `Option` に使わない**——複数行に割れる（S13-a の BAR で
    // 踏んだのと同じ癖）。
    let kernel_checksum_shown = match kernel_checksum {
        Some(value) => format!("{value:#010x}"),
        None => "none".to_string(),
    };
    println!(
        "{context}: checksum from the device = {kernel_checksum_shown}, from the image file = \
         {expected_checksum:#010x}; match = {checksum_matches}"
    );

    // capacity（512 バイト単位の数）も独立に突き合わせる。出所はファイルの長さ。
    let capacity_line = serial.lines().find(|l| l.contains("capacity="));
    let kernel_capacity = capacity_line.and_then(|l| parse_marked_u64(l, "capacity="));
    let expected_capacity = image.len() as u64 / 512;
    let capacity_matches = kernel_capacity == Some(expected_capacity);
    println!(
        "{context}: capacity from the device = {kernel_capacity:?}, file length / 512 = \
         {expected_capacity}; match = {capacity_matches}"
    );

    if !checksum_matches || !capacity_matches {
        bail!("{context}: the sector the kernel read does not agree with the image file");
    }
    Ok(())
}

/// `marker` の直後の `0x` 付き 16 進を拾う。
fn parse_marked_hex(line: &str, marker: &str) -> Option<u32> {
    let rest = line.split(marker).nth(1)?;
    let token = rest.split_whitespace().next()?;
    u32::from_str_radix(token.trim_start_matches("0x"), 16).ok()
}

/// `marker` の直後の 10 進を拾う。
fn parse_marked_u64(line: &str, marker: &str) -> Option<u64> {
    let rest = line.split(marker).nth(1)?;
    let token = rest.split_whitespace().next()?;
    token.parse().ok()
}

/// 割り込みの実演の判定行を待ち、届いた数を確かめる（S13-d）。
///
/// **主張は「届いて数えられる」ことである。** 数はカーネルのカウンタだが、
/// 破壊（エッジのまま配線する）が届かなくなることは、判定行が出ずに
/// 上限つき待ちの停止で観測される——**「レジスタは書けてしまい何も落ちない」
/// 形を、届いた数の判定だけが観測へ変える。**
fn cmd_virtio_irq_test(features: &[&str]) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let tag = if features.is_empty() {
        "default".to_string()
    } else {
        features.join("-")
    };
    let serial_log = workspace_root
        .join("target")
        .join(format!("virtio-irq-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });
    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the virtio irq test")?;

    let exercise_marker = "virtio-blk: interrupt exercise:";
    let error_marker = "virtio-blk: the interrupt exercise failed";
    // **判定が見る最後の行はこれである**（d-2。BKL を解いてから待ったこと）。
    let released_marker = "blocking read: released the BKL before waiting";
    // **演習の行で切ると、その後に出る行を取り逃がす（e-4 で踏んだ）。**
    //
    // **判定は 2 本の行を見ているのに、待っていたのは 1 本目だけだった。**
    // **カーネルが太って遅くなった日に、2 本目が間に合わなくなった**
    // ——`--full` が「BKL を解いていない」と出したが、**実際にはログが
    // そこで切れていた**（`docs/troubleshooting.md`）。
    // **S12-d の `fs-image-ready` と同じ族である**——**判定が見る時点と、
    // 判定したい対象が生まれる時点が違う。**
    //
    // **2 本目を待つ。** **破壊の構成では 2 本目が出ないことがある**ので、
    // **演習の行を見たら猶予を置いて切る**（上限は全体の締切より短い）。
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let mut grace: Option<Instant> = None;
    while Instant::now() < deadline {
        let text = read_lossy(&serial_log);
        if text.contains(released_marker) || text.contains(error_marker) {
            break;
        }
        if text.contains(exercise_marker) {
            let until = *grace.get_or_insert_with(|| Instant::now() + VIRTIO_IRQ_GRACE);
            if Instant::now() >= until {
                break;
            }
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let context = if features.is_empty() {
        "virtio-irq".to_string()
    } else {
        format!("virtio-irq {}", features.join("+"))
    };
    let context = context.as_str();

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu_debug = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu_debug, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    // 配線の読み戻し（level / active-low がハードウェアに載っていること）。
    let routed = serial
        .lines()
        .find(|l| l.contains("ioapic: virtio IRQ"))
        .map(|l| l.trim().to_string());
    if let Some(line) = &routed {
        println!("{context}: {line}");
    }
    // **一致の判定はカーネルが宣言と突き合わせた結果を読む。** 期待値
    // （level / active-low の組）は platform の宣言（MADT の override）から
    // カーネル側が導いており、こちらは値を定数で持たない。
    let route_ok = routed
        .as_deref()
        .is_some_and(|l| l.contains("matches the platform's declaration = true"));

    let Some(line) = serial.lines().find(|l| l.contains(exercise_marker)) else {
        if let Some(error_line) = serial.lines().find(|l| l.contains(error_marker)) {
            bail!(
                "{context}: the exercise did not complete: {}",
                error_line.trim()
            );
        }
        bail!("{context}: the kernel reported neither the exercise line nor an error");
    };
    println!("{context}: {}", line.trim());
    let delivered = parse_marked_u64(line, "delivered=");
    let not_mine = parse_marked_u64(line, "not-mine=");
    let counts_ok = delivered == Some(2) && not_mine == Some(0);
    println!(
        "{context}: delivered = {delivered:?} (wanted 2), not mine = {not_mine:?} (wanted 0), \
         route read back matching the platform's declaration = {route_ok}"
    );
    // d-2: BKL を解いてから待ったこと（§6。ADR-0036）。
    let released = serial
        .lines()
        .any(|l| l.contains("blocking read: released the BKL before waiting"));
    println!("{context}: the blocking read released the BKL before waiting = {released}");

    if !counts_ok || !route_ok || !released {
        bail!("{context}: the interrupt did not arrive the way the route claims");
    }
    Ok(())
}

fn cmd_shell_test(mode: ShellTestMode) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, mode.features())?;
    // **`KEYMAP=us` の回は `disk0.img` を作り直さない（f-1b）。**
    // **1 度目の起動が `zi` で書き込んだものなので、作り直すと消える。**
    let esp_dir = if mode.keeps_the_disk() {
        stage_esp_keeping_the_disk(&workspace_root, &bootloader_efi, &kernel_elf)?
    } else {
        stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?
    };

    let serial_log = workspace_root
        .join("target")
        .join(mode.serial_log_name().as_str());
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);
    let monitor_socket = PathBuf::from(format!(
        "/tmp/zaytos-xtask-shell-{}.sock",
        std::process::id()
    ));
    let _ = fs::remove_file(&monitor_socket);
    ensure_socket_path_fits(&monitor_socket)?;

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: Some(&monitor_socket),
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the shell test")?;

    // **プロンプトが出るまで待つ。上限つき。**
    let ready_marker = "zash: ready";
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let mut ready = false;
    while Instant::now() < deadline {
        if read_lossy(&serial_log).contains(ready_marker) {
            ready = true;
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    if ready {
        match connect_monitor_with_retry(&monitor_socket) {
            Ok(mut stream) => {
                // **到達条件の 3 つを順に打つ。** そのあと `exit` で締める。
                // **`slash` と `spc` と `minus` は monitor のキー名である。**
                for line in SHELL_TEST_LINES {
                    for key in *line {
                        if writeln!(stream, "sendkey {key}").is_err() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(120));
                    }
                    // **子が走り終えるのを待つ。** `ls` と `cat` は
                    // `spawn` で起こされ、終わるまでシェルは戻らない。
                    thread::sleep(Duration::from_millis(800));
                }
            }
            Err(e) => println!("shell-test: could not reach the QEMU monitor: {e}"),
        }
        // **起こし直しが判定行に出るまで待つ。**
        // **3 つのコマンドを走らせた後なので、締めの `exit` が効いてから
        // `init` が像を読み直すまでに時間がかかる。**
        thread::sleep(Duration::from_secs(8));
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&monitor_socket);

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);

    let context = mode.context();
    let context = context.as_str();
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        bail!("{context}: the kernel did not start");
    }

    println!("--- {context}: relevant output ---");
    for line in serial.lines().filter(|l| {
        l.contains("init:") || l.contains("sh:") || l.contains("zaytos$") || l.contains("spawn:")
    }) {
        println!("{line}");
    }
    println!("--- end ---");

    // **合図である（2026-09-04）。** **プロンプトが出ていなければ、
    // この下の判定はすべて範囲を取れない**（`after_shell` が空になる）。
    println!("{context}: (signal) the shell printed its prompt = {ready}");

    // **シェルが出た後だけを見る（S12 前の手当ての 1 本目）。**
    //
    // **`ls`・`cat`・`hello` は起動シーケンスでも走っている**——`syscall-test` が
    // `spawn` で起こす。**シリアル全体を `contains` で見ると、シェルが 1 つも
    // 起こさなくても真になる。** 実際そうなっていた（下の 3 つ）。
    //
    // **到達条件はシェルについての主張なので、シェルが出た後の範囲で見る。**
    //
    // **`ready` が偽なら範囲が取れない。** そのときは空にして下の判定をすべて
    // 偽にする——**プロンプトが出ていないなら、シェルは何も起こしていない。**
    let after_shell = serial
        .find(ready_marker)
        .map(|at| &serial[at..])
        .unwrap_or("");

    // **打鍵が Ring 3 まで届き、組み込みの `exit` が効いたこと。**
    //
    // **この 2 つは範囲を絞らない。** `init` の行はシェルが出た後にしか現れず、
    // **`restart 1 of 3` は起こし直した回数を含んでいる**ので、
    // **起動シーケンスでは真にならない。**
    let ended = serial.contains("init: the shell ended (Exited(0))");
    // **`init` が起こし直したこと。**
    let restarted = serial.contains("init: starting /bin/zash (restart 1 of 3)");
    // **起こし直したのはちょうど 1 回であること（S12 前の手当て、C）。**
    //
    // **シェル自身が死ぬと、ここが 2 回になる。** 締めの `exit` でどのみち
    // 1 回起きるので、**「起こし直した」だけでは足りない**——
    // **深さ 1 でも畳む破壊は、1 回目の判定を通ってしまう**（実測でそうなった）。
    let restarted_only_once = !serial.contains("init: starting /bin/zash (restart 2 of 3)");
    // **打った文字が反響していること。** シェルが反響を出しているので、
    // **Ring 3 まで届いた証拠が出力そのものにある。**
    // **色の列を落とした写しで見る（ES-d）。** プロンプトの直後に打った語が
    // 続くことを見る 3 つは、**色が付いた時点で素のログには当たらない**
    // （[`strip_ansi`] の doc に、隠したものを誰が見ているかを書いてある）。
    let after_shell_plain = strip_ansi(after_shell);
    let after_shell_plain = after_shell_plain.as_str();
    let echoed = after_shell_plain.contains("zaytos$ /bin/ls");
    // **到達条件の 3 つ。** 出力そのものがシリアルに現れる。
    let ran_ls = after_shell.contains("lost+found");
    let ran_cat = after_shell.contains("welcome to ZaytOS");
    let ran_hello = after_shell.contains("hello from ring 3");
    // **C で書いたプログラムが走ったこと（C-a。`ADR-0057`）。**
    //
    // **落とす破壊は無い。** **到達条件だからである**——**`hello ran` と同じ族で、
    // あちらも「落とす破壊が無い判定」の一覧に載っている**
    // （`docs/verification-coverage.md` の「判定の側から見る」）。
    // **足すときにその場で確かめた**（`docs/coding-standards.md` の
    // 「判定を足すときは、その場で『落とす破壊が在るか』を確かめる」）。
    let ran_c_hello = after_shell.contains("hello from C");
    // **自前の libc がヒープを取れたこと（C-c。`ADR-0057`）。**
    //
    // **ホストの単体テストでは覆えない面である**——**あちらは純粋な関数だけで、
    // `brk` はシステムコールである。** **ここでしか主張できない。**
    //
    // **落とす破壊は無い。** **`brk-skip-shrink-test` は縮める側を壊すもので、
    // 伸ばす側は通る**（実測でこの行は緑のままである）。**足すときに確かめた。**
    let c_heap_worked = after_shell.contains("heap ok");

    // **`/` を含まない語が `/bin/` の下で見つかること（S12 前の手当ての 3 本目）。**
    //
    // **`ls` と `cat /etc/motd` を `/bin/` を付けずに送っている。**
    // **反響でその行が打たれたことを見て、`cannot run` が出ていないことで
    // 起こせたことを見る。**
    //
    // **出力そのもの（`lost+found` など）では区別できない**——
    // **`/bin/` を付けた側が同じものを出す。** 上の 3 判定と同じ理由である。
    let typed_bare_ls = after_shell_plain.contains("zaytos$ ls\n");
    let typed_bare_cat = after_shell_plain.contains("zaytos$ cat /etc/motd\n");
    // **「1 つも `cannot run` が出ていない」では見られなくなった（S12 前の手当て）。**
    // **行編集の判定が、わざと走らない語（`abx`）を打つためである。**
    // **名前を挙げて見る形へ狭めた**——ここが主張したいのは
    // 「`/bin/` を付けずに打った 3 つが起こせたこと」だけである。
    let no_cannot_run = !after_shell.contains("zash: ls: cannot run")
        && !after_shell.contains("zash: cat: cannot run")
        && !after_shell.contains("zash: spawn-test: cannot run");
    let bare_names_resolved = typed_bare_ls && typed_bare_cat && no_cannot_run;

    // **Backspace が行を編集したこと（S12 前の手当て）。**
    //
    // **打ったのは `abc` → Backspace → `x` で、走るのは `abx` である。**
    // **消えていなければ `abcx` になる。** 出る側と出ない側の両方を見る——
    // **片方だけだと、シェルが行を空にしてしまっても通る。**
    let edited_line_ran = after_shell.contains("zash: abx: cannot run");
    let unedited_line_absent = !after_shell.contains("zash: abcx: cannot run");
    let backspace_edited_the_line = edited_line_ran && unedited_line_absent;

    // **左矢印が挿入点を動かしたこと（S12 前の手当て）。**
    //
    // **打ったのは `pq` → 左 → `y` で、走るのは `pyq` である。**
    // **動いていなければ `pqy` になる。** Backspace と同じ形で、出る側と
    // 出ない側の両方を見る。
    //
    // **破壊ビルドでは期待が裏返る**（[`ShellTestMode`]）。
    // **どちらの向きでも 2 本で見る**——片方だけだと、シェルが行を
    // 空にしてしまった場合に通ってしまう。
    let moved = after_shell.contains("zash: pyq: cannot run");
    let did_not_move = after_shell.contains("zash: pqy: cannot run");
    let arrow_behaved_as_expected = if mode.expects_the_cursor_to_move() {
        moved && !did_not_move
    } else {
        did_not_move && !moved
    };

    // **Esc が Ring 3 へ届いたこと（zi-a）。**
    //
    // **打ったのは `m` → Esc → `[` → `D` → `n` である。** `\x1b` が届いて
    // いれば zash が 3 打を CSI（左）として解釈し、走るのは `nm` である。
    // **届いていなければ `[` と `D` が字のまま入り、`m[Dn` になる。**
    // 左矢印の判定と同じ形で、出る側と出ない側の両方を見る。
    //
    // **破壊ビルド（`keyboard-drop-esc-test`）では期待が裏返る。**
    let esc_moved = after_shell.contains("zash: nm: cannot run");
    // **届かなかったときに残る語は、変換表で変わる（f-1b）。**
    // **台本は `bracket_right`（`0x1B`）を打っており、JIS では `[`、
    // US では `]` である。**
    let esc_did_not_move = if mode.expects_the_us_layout() {
        after_shell.contains("zash: m]Dn: cannot run")
    } else {
        after_shell.contains("zash: m[Dn: cannot run")
    };
    let esc_behaved_as_expected = if mode.expects_esc_to_reach_ring3() {
        esc_moved && !esc_did_not_move
    } else {
        esc_did_not_move && !esc_moved
    };

    // **上下の矢印の判定は SE-c で置き換えた。**
    //
    // **以前は「行が壊れないこと」を見ていた**（zi-a。`u` → 上 → `v` → 下 → `w` で
    // `uvw` が走る）。**あれは「上下を消費して観測できる者が居ない」ために、
    // 届いたことの証明にならない判定だった**——そう書いてあった。
    // **SE-c で消費する者ができたので、履歴を辿れることを直接見る形へ移した**
    // （下の `history_walked_with_arrows`）。**いまは届いたことの証明である。**
    // **変換表の外に居る 2 キーが Ring 3 まで届いたこと（zi-e）。**
    //
    // **`ろ`（`0x73`）と `¥`（`0x7D`）はどちらも `\` を出す**ので、走るのは
    // `\\` である。**出る側と、片方だけになった側の両方を見る。**
    //
    // **なぜ実機の側にも置くのか。** **ホストの単体テストが固定したのは表であって、
    // 打鍵が実機で届くことではない。** **表に在るが経路が繋がっていない形は、
    // ホストからは見えない**（`character_for` は正しくても、`TABLE_LEN` の
    // 外に居る 2 つは、範囲の判定より先に引く経路が要る）。
    let jis_only_keys_reached_ring3 = after_shell.contains("zash: \\\\: cannot run");
    let only_one_jis_key_arrived = after_shell.contains("zash: \\: cannot run");

    // **Ctrl+C が打ちかけの行を捨てたこと（S12 前の手当て、C。深さ 1）。**
    //
    // **`zz` と打ってから Ctrl+C を送り、`ret` を打っている。**
    // **捨てられていれば `zz` は走らない。** 反響の `^C` が出ることも見る——
    // **片方だけだと、シェルが打鍵を受け取っていなくても通る。**
    let ctrl_c_echoed = after_shell.contains("^C");
    let discarded_line_did_not_run = !after_shell.contains("zash: zz: cannot run");
    let ctrl_c_discarded_the_line = ctrl_c_echoed && discarded_line_did_not_run;

    // **Ctrl+C が回り続ける子を止めたこと（S12 前の手当て、C。深さ 2）。**
    //
    // **`spin` はシステムコールを出さずに回る。** **走り始めたこと**（あちらが
    // 出す 1 行）と、**止まったこと**（シェルの `interrupted` と、カーネルの
    // 判定行）の両方を見る。
    //
    // **走り始めたことを見なければ、「起きなかった」を「止めた」と誤読する。**
    let spin_started = after_shell.contains("spin: running");
    let shell_saw_the_interruption = after_shell.contains("interrupted");
    let kernel_reported_the_interruption = after_shell.contains("interrupt: stopped /bin/spin");
    // **止めた打鍵そのものが残っていないこと。**
    //
    // **`^C` はちょうど 1 つである**——深さ 1 で捨てた行の分だけである。
    // **子を止めた側の Ctrl+C はスキャンコードのリングにも積まれている**ので、
    // **捨てないと、止めた直後のプロンプトへ 2 つ目が出る**（実測でそうなった）。
    let echoed_ctrl_c_count = after_shell.matches("^C").count();
    let ctrl_c_stopped_the_child = spin_started
        && shell_saw_the_interruption
        && kernel_reported_the_interruption
        && echoed_ctrl_c_count == 1;

    // **Home と End が挿入点を端へ動かしたこと（SE-b。`ADR-0050`）。**
    //
    // **打ったのは `ab` → Home → `c` → End → `d` で、走るのは `cabd` である。**
    // **動いていなければ `abcd` になる。** 出る側と出ない側の両方を見る。
    let home_end_moved = after_shell.contains("zash: cabd: cannot run");
    let home_end_did_not_move = after_shell.contains("zash: abcd: cannot run");
    let home_and_end_moved_the_insertion_point = home_end_moved && !home_end_did_not_move;

    // **Ctrl+A と Ctrl+E が同じ動きをしたこと（SE-b。`ADR-0050`）。**
    //
    // **打ったのは `ef` → Ctrl+A → `g` → Ctrl+E → `h` で、走るのは `gefh` である。**
    // **一般化が外れていれば `a` と `e` が字として入り、`efageh` になる。**
    //
    // **Home / End と別の 1 本にしてある。** **落ちる破壊が違う**——
    // あちらは `keyboard-drop-home-end-test`、こちらは
    // `keyboard-drop-ctrl-letters-test` である。
    let ctrl_ae_moved = after_shell.contains("zash: gefh: cannot run");
    let ctrl_ae_typed_letters = after_shell.contains("zash: efageh: cannot run");
    let ctrl_a_and_e_moved_the_insertion_point = ctrl_ae_moved && !ctrl_ae_typed_letters;

    // **Delete が挿入点の字を消したこと（SE-b）。**
    //
    // **打ったのは `ij` → 左 → Delete で、走るのは `i` である。**
    // **扱っていなければ `i3~j` になる**——**実測でその形だった**（2026-08-28）。
    //
    // **矢印が落ちている構成では期待が変わる。** **左が届かないので挿入点は
    // 行末のままで、Delete は消す字を持たない**——走るのは `ij` である。
    // **実測でここを踏んだ**（2026-08-28。台本へ左を足した回で
    // `keyboard-drop-arrows` の実行が落ちた）。**台本を変えるときは、
    // 台本を写している判定を数え直すこと。**
    let delete_removed = after_shell.contains("zash: i: cannot run");
    let delete_left_the_csi = after_shell.contains("zash: i3~j: cannot run");
    let delete_left_the_line_alone = after_shell.contains("zash: ij: cannot run");
    let delete_removed_the_character = if mode.expects_the_cursor_to_move() {
        delete_removed && !delete_left_the_csi
    } else {
        delete_left_the_line_alone && !delete_removed
    };

    // **Tab が行へ入らないこと（SE-b。`ADR-0050`）。**
    //
    // **打ったのは `k` → Tab → `l` で、走るのは `kl` である。**
    // **捨てていなければ `0x09` が語に混ざる。**
    //
    // # 前提が TAB-1 で変わった（2026-09-05）
    //
    // **`ADR-0056` まで、Tab は捨てられていた。** **いまは補完が走る**
    // ——**`k` で始まる名前が `/bin` に無いので候補が 0 本になり、
    // 行は変わらないまま `0 matches` が出る。**
    //
    // **判定は同じ形で通る**（実測）**が、通る理由が変わった。**
    // **「捨てているから入らない」ではなく「補完が行を変えなかったから
    // 入らない」である。** **`0x09` が語に混ざらないことは、どちらでも
    // 主張できている。**
    //
    // **`/bin` に `k` で始まる名前を置くと、この判定は補完の結果を見る
    // ことになる**——**そのときは打つ字を変えること。**
    // **族は `docs/troubleshooting.md` の「能力を足すと、既存の判定の
    // 前提が消える」である**（**これは 5 例目で、初めて落ちずに意味だけが
    // 変わった**）。
    let tab_dropped = after_shell.contains("zash: kl: cannot run");
    let tab_kept = after_shell.contains("zash: k\tl: cannot run");
    let tab_stayed_out_of_the_line = tab_dropped && !tab_kept;

    // **受け手が決まっていない Ctrl+英字が行へ入らないこと（SE-b。`ADR-0050`）。**
    //
    // **打ったのは `n` → Ctrl+G → `o` で、走るのは `no` である。**
    // **捨てていなければ `0x07` が語に混ざる。**
    //
    // **SE-f で Ctrl+D から Ctrl+G へ移した**——**あちらに意味ができたので、
    // 「捨てる」を主張する鍵として使えなくなった。**
    //
    // **Tab と別の 1 本にしてある**——**落ちる破壊が違う**（台本の doc）。
    let unknown_ctrl_dropped = after_shell.contains("zash: no: cannot run");
    let unknown_ctrl_kept = after_shell.contains("zash: n\u{7}o: cannot run");
    let unknown_ctrl_stayed_out_of_the_line = unknown_ctrl_dropped && !unknown_ctrl_kept;

    // **`$NAME` が展開されたこと（SE-d。`ADR-0049`）。**
    //
    // **打った行と出た行を「続けて」見ることはできない。** **実測で踏んだ**
    // （2026-08-28）——**`spawn` と `user-load` の INFO が両者の間に何行も入る。**
    // **反響と出力は別々に見る。**
    //
    // **語の数はカーネルが出している。** `/bin/echo` を起こすたびに
    // `initial stack at ... (argc=N, ...)` が出るので、**渡った語の数がそのまま読める**
    // ——**出力の空白を数えるより強い観測である**（`echo` の書き方に依らない）。
    let echo_argcs: Vec<usize> = after_shell
        .lines()
        .filter(|line| line.contains("/bin/echo initial stack"))
        .filter_map(|line| line.split("argc=").nth(1))
        .filter_map(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .filter_map(|digits| digits.parse().ok())
        .collect();
    // **添字ではなく名前で引く（f-2 で直した）。**
    //
    // **以前は生の添字だった**（`echo_argcs.get(4)` のように書いてあった）。
    // **f-1 と f-1b が台本の途中へ `echo` の行を 4 つ足したとき、`Ctrl+W` と
    // `$1` を見ている 2 本の添字がずれた**——**ずれた先の行の `argc` が
    // たまたま同じ 2 だったので、判定は緑のままだった**（実測。2026-08-31）。
    // **「偶然に頼った捕捉は捕捉ではない」の実例である。**
    //
    // **名前で引き、本数も突き合わせる**——**台本へ `echo` の行を足して
    // ここへ名前を足さなければ、本数の判定が落ちる。** **静かにずれる形を、
    // 落ちる形へ変えた。**
    let echo_lines_are_accounted_for = echo_argcs.len() == ECHO_LINES_IN_ORDER.len();
    let echo_argc = |name: &str| -> Option<usize> {
        let index = ECHO_LINES_IN_ORDER
            .iter()
            .position(|entry| *entry == name)?;
        echo_argcs.get(index).copied()
    };
    // **最初の 4 行は展開の規則を見ている**（`ADR-0049` の判定）。
    // **落とす形なら 2 / 3 / 1 / 3 である。**
    let echo_word_counts = echo_argc("echo $PATH") == Some(2)
        && echo_argc("echo a $UNSET b") == Some(3)
        && echo_argc("echo $UNSET") == Some(1)
        && echo_argc("echo a$TERM b") == Some(3);

    // **f-2（`export` と `set`。`ADR-0053`）の判定 5 本。**
    //
    // **`envc` も `argc` と同じ行から読める**（`user-load` の 1 行に両方が出る）。
    //
    // # 期待値を像の状態から切り離す（f-2 の後に直した）
    //
    // **最初は「3 から 4 へ」と書いていた。** **`keymap (us)` の回は像を作り直さない
    // ので、`KEYMAP=us` が入って 4 から始まる**——**落ちた**（実測。2026-08-31。
    // **`--full` でしか出ない形である**）。**差で見れば像に依らない。**
    //
    // **主張は「1 本増えたこと」である**——**「子へ届いた」を主張しているのは
    // これだけで、判定 1（`echo $ZF2` が値を出す）は主張しない**（展開はシェルが
    // 行うので、子が何も知らなくても値は出る）。
    let echo_envcs: Vec<usize> = after_shell
        .lines()
        .filter(|line| line.contains("/bin/echo initial stack"))
        .filter_map(|line| line.split("envc=").nth(1))
        .filter_map(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .filter_map(|digits| digits.parse().ok())
        .collect();
    let envc_at = |name: &str| -> Option<usize> {
        let index = ECHO_LINES_IN_ORDER
            .iter()
            .position(|entry| *entry == name)?;
        echo_envcs.get(index).copied()
    };
    let envc_before_export = envc_at("echo $ZF2 (before export)");
    let envc_after_export = envc_at("echo $ZF2 (after export)");
    // **US の回は `export` が断られるので、増えない。** **両側を見る。**
    let wanted_envc_growth = usize::from(mode.expects_the_export_script());
    let the_child_saw_the_exported_name = match (envc_before_export, envc_after_export) {
        (Some(before), Some(after)) => after.checked_sub(before) == Some(wanted_envc_growth),
        _ => false,
    };

    // **判定 1**——**`export` した名前がシェルの表から引けること。**
    // **打つ前は語が 0 個になり、打った後は 2 個になる。**
    // **US の回はどちらも 0 個である**（置けていないので空へ落ちる）。
    let expanded_the_exported_name = if mode.expects_the_export_script() {
        after_shell.contains("\nexported\n")
            && echo_argc("echo $ZF2 (before export)") == Some(1)
            && echo_argc("echo $ZF2 (after export)") == Some(2)
    } else {
        !after_shell.contains("\nexported\n")
            && echo_argc("echo $ZF2 (before export)") == Some(1)
            && echo_argc("echo $ZF2 (after export)") == Some(1)
    };

    // **判定 2**——**`set` が表を並べること。**
    // **`ZF2` だけを見ない**——**源から来た 3 本も出ていることを同時に見る。**
    // **US の回は `ZF2` が入らない**ので、そちら側を見る。
    let set_listed_the_source = after_shell.contains("\nTERM=zaytos\n")
        && after_shell.contains("\nPATH=/bin\n")
        && after_shell.contains("\nHOME=/root\n");
    let set_listed_the_table = set_listed_the_source
        && (after_shell.contains("\nZF2=exported\n") == mode.expects_the_export_script());

    // **判定 5**——**断ったことが人に見えること**（`ADR-0046` のエコー領域）。
    // **ホストテストは「断る」までしか言わない。** **見えることは別の主張である。**
    //
    // **US の回は 1 行目から断られる**（`=` が `_` になるため）。**文言は同じで、
    // 断られる語が違う。**
    let export_refused_a_bad_name = if mode.expects_the_export_script() {
        after_shell.contains("zash: export: 1BAD=x: not a name")
    } else {
        after_shell.contains("zash: export: ZF2_exported: not a name")
    };

    // **判定 4**——**`export` がシェル自身の振る舞いを変えること**
    // （`ADR-0053` の Decision 6。**控えたままだと効かない**）。
    //
    // **`PATH` を壊すと `hello` が起こせなくなり、戻すと起こせる。**
    // **`hello` は既定の回で 2 度走る**——最初の `/bin/hello` と、`PATH` を戻した後である。
    // **US の回は `PATH` が壊れないので 3 度走る**（`export` が断られる）。
    let hello_runs = after_shell.matches("/bin/hello initial stack").count();
    let export_changed_the_path = if mode.expects_the_export_script() {
        after_shell.contains("zash: hello: cannot run") && hello_runs == 2
    } else {
        !after_shell.contains("zash: hello: cannot run") && hello_runs == 3
    };

    let typed_echo_path = after_shell_plain.contains("zaytos$ echo $PATH\n");
    let expanded_a_value = typed_echo_path && after_shell.contains("\n/bin\n");
    // **丸ごと空になった語が落ちたこと。** **空白の数を見る。**
    // **`argc` の並びも同時に見る**——**空白は `echo` の書き方に依るが、
    // `argc` は依らない。2 つは独立である。**
    let dropped_the_empty_word = after_shell.contains("\na b\n");
    let kept_the_empty_word = after_shell.contains("\na  b\n");
    let empty_word_was_dropped = dropped_the_empty_word && !kept_the_empty_word && echo_word_counts;
    // **語が 0 個になったこと。** **`argc` が 1（`echo` 自身だけ）である。**
    let expanded_to_nothing = echo_argc("echo $UNSET") == Some(1);
    // **`$` の直後以外の字が壊れないこと。**
    let expanded_inside_a_word =
        after_shell.contains("\nazaytos b\n") && echo_argcs.get(3) == Some(&3);

    // **履歴を矢印で辿れること（SE-c）。**
    //
    // **`aa` を打ち、`bb` を打ち、上 上 で `aa` へ戻って Enter した。**
    // **辿れていれば `aa` は 2 度走る。** **積んでいなければ 1 度きりである。**
    //
    // **矢印が落ちている構成では期待が変わる**——上が届かないので 1 度きりになる
    // （Delete の判定と同じ形である）。
    let ran_aa = after_shell.matches("zash: aa: cannot run").count();
    let history_walked_with_arrows = if mode.expects_the_cursor_to_move() {
        ran_aa == 2
    } else {
        ran_aa == 1
    };
    // **同じ履歴を `Ctrl+P` / `Ctrl+N` で辿れること（SE-c）。**
    //
    // **2 つ戻って 1 つ進むので `dd` が 2 度走る。**
    // **上下と同じ関数を通しているが、それは判定になっていない**ので別に置く。
    let history_walked_with_ctrl = after_shell.matches("zash: dd: cannot run").count() == 2;
    // **`Ctrl+B` と `Ctrl+F` が左右へ動かすこと（SE-c）。**
    let ctrl_bf_moved = after_shell.contains("zash: rtsu: cannot run");
    let ctrl_bf_typed_letters = after_shell.contains("zash: rsbtfu: cannot run");
    let ctrl_b_and_f_moved_the_insertion_point = ctrl_bf_moved && !ctrl_bf_typed_letters;

    // **`LINE_MAX` ちょうどの行が畳まずに断られること（SE-c。`d7de0ce`）。**
    //
    // **`z` を 128 打った。** **入るのは 127 までで、128 打目は溢れる。**
    // **直す前は配列の外を書いて畳まれ、シェルが止まっていた。**
    //
    // **「止まらなかったこと」は下の判定が全部見ている**（止まればすべて落ちる）。
    // **ここが見るのは「断り書きが出たこと」である。**
    let long_line_was_refused = after_shell.contains("zash: line too long");

    // **`Ctrl+W` が直前の語を消したこと（SE-f）。**
    //
    // **`echo qq rr` の末尾で打った。** **切れ目は空白だけなので `qq ` が残り、
    // 出るのは `qq` で `argc` は 2 である。** **効いていなければ `qq rrw` になる。**
    let ctrl_w_output = after_shell.contains("\nqq\n");
    let ctrl_w_left_the_word = after_shell.contains("\nqq rrw\n");
    let ctrl_w_deleted_the_word =
        ctrl_w_output && !ctrl_w_left_the_word && echo_argc("echo qq rr + Ctrl+W") == Some(2);

    // **`$` の直後が名前の先頭でなければ字として残ること（SE-f。`ADR-0049` の 5）。**
    //
    // **`echo $1` を打った。** **出るのは `$1` そのものである。**
    let dollar_stayed_literal = after_shell.contains("\n$1\n") && echo_argc("echo $1") == Some(2);

    // **`~` が `HOME` へ展開されること（f-1。`ADR-0049` の Addendum）。**
    //
    // **3 本を 1 つにまとめる。** **どれが落ちても「`~` の展開が壊れた」の
    // 1 つの主張である。**
    //
    // **期待値を定数で持たない。** **`HOME` の値はカーネルが積んだもので、
    // シリアルの `env-source:` の側からは読めない**ので、
    // **`echo $HOME` と `echo ~` が同じ物を出すことを見る**——
    // **源が独立である**（片方は `$NAME` の展開、片方は `~` の展開）。
    let tilde_alone = after_shell.contains("\n/root\n");
    let tilde_with_path = after_shell.contains("\n/root/x\n");
    let tilde_inside_a_word = after_shell.contains("\na~b\n");
    let tilde_expanded = (tilde_alone && tilde_with_path && tilde_inside_a_word)
        == mode.expects_the_tilde_to_expand();

    // **変換表が実行時に選ばれていること（f-1b）。**
    //
    // **既定は JIS なので `@+` が出る。** **`KEYMAP=us` の回では `[:` である。**
    // **期待は構成から決まる**（[`ShellTestMode::expects_the_us_layout`]）。
    let layout_output = if mode.expects_the_us_layout() {
        "\n[:\n"
    } else {
        "\n@+\n"
    };
    let layout_is_in_use = after_shell.contains(layout_output);

    // **`Ctrl+K` が挿入点から行末まで消したこと（SE-f）。**
    let ctrl_k_cut = after_shell.contains("zash: kk: cannot run");
    let ctrl_k_left_the_line = after_shell.contains("zash: kkxx: cannot run");
    let ctrl_k_cut_to_the_end = ctrl_k_cut && !ctrl_k_left_the_line;

    // **`Ctrl+U` が行頭から挿入点まで消したこと（SE-f）。**
    //
    // **`Ctrl+K` と範囲の計算が逆なので、別の 1 本である。**
    let ctrl_u_cut = after_shell.contains("zash: yy: cannot run");
    let ctrl_u_left_the_line = after_shell.contains("zash: uuyy: cannot run");
    let ctrl_u_cut_to_the_start = ctrl_u_cut && !ctrl_u_left_the_line;

    // **`Ctrl+D` が挿入点の字を消したこと（SE-f）。**
    let ctrl_d_cut = after_shell.contains("zash: mmy: cannot run");
    let ctrl_d_left_the_line = after_shell.contains("zash: mxmy: cannot run");
    let ctrl_d_deleted_the_character = ctrl_d_cut && !ctrl_d_left_the_line;

    // **空行の `Ctrl+D` が何もしないこと（SE-f）。**
    //
    // **`bash` はここで EOF になりシェルが終わる。** **続けて打った `ee` が
    // そのまま走ることで、終わっていないことを見る。**
    // **終わっていれば「ちょうど 1 回」の判定も落ちる**ので、2 本で見ている。
    let ctrl_d_on_an_empty_line_did_nothing = after_shell.contains("zash: ee: cannot run");

    // **`Ctrl+L` が画面を消して描き直したこと（SE-f）。**
    //
    // **行が消えないことは `ll` が走ることで見る。**
    // **消す並びを出したことは、シリアルに出た並びそのもので見る。**
    //
    // **画面が実際に消えたことは見ていない。** **`--shell-test` は実打鍵の経路で、
    // 画面を観測する仕組み（`screen-text`）はカーネル側の台本にしか無い。**
    // **ED(2) と CUP の解釈は `--ansi-test` が持っている**（`ADR-0029`）。
    // **観測していないことは観測していないと書く。**
    let ctrl_l_kept_the_line = after_shell.contains("zash: ll: cannot run");
    let ctrl_l_cleared = serial.contains("\u{1b}[2J\u{1b}[H");
    let ctrl_l_redrew_the_screen = ctrl_l_kept_the_line && ctrl_l_cleared;

    // **辿ってから戻ると、打ちかけの行が復ること（SE-c の宿題）。**
    //
    // **`st` と打ってから 上 → `Ctrl+N` で戻した。** **復らなければ、
    // 辿った先の行か空行になる。**
    let the_pending_line_came_back = after_shell.contains("zash: st: cannot run");

    // **同じ行が履歴に 2 つ並ばないこと（SE-c の宿題）。**
    //
    // **`oo` `pp` `pp` と打ってから 上 上 した。** **並んでいなければ `oo` へ届く。**
    // **矢印が落ちている構成では辿らないので、`oo` は 1 度きりである。**
    let ran_oo = after_shell.matches("zash: oo: cannot run").count();
    let duplicates_were_not_stored = if mode.expects_the_cursor_to_move() {
        ran_oo == 2
    } else {
        ran_oo == 1
    };

    // **カーネルスタックに余裕が残っていること（P-c-1 の手当て）。**
    //
    // **ガードは真偽しか言わない**——**「踏んだか」は分かるが「どれだけ
    // 余っているか」は分からない。** **緑であることと、余裕があることは違う**
    // （`ADR-0046` が「余裕が無い」と書いたのに、数を誰も見ていなかった）。
    //
    // **16KiB の根拠は実測である**——**関数 1 つの枠は最大 4KiB で
    // （`kernel_main` を除く）、4 つ積んでも足りる幅である。**
    //
    // **高水位は揺れない**（起動シーケンスは決定的である。2 回続けて同じ値を
    // 実測した）ので、判定に載せられる。
    let stack_spare = serial
        .lines()
        .find(|line| line.contains("stack-water: the kernel stack used"))
        .and_then(|line| line.split("byte(s); ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|token| token.parse::<usize>().ok());
    let kernel_stack_has_room = stack_spare.is_some_and(|spare| spare >= KERNEL_STACK_MIN_SPARE);

    // **遠征スタックにも余裕が残っていること（f-2 の後の手当て）。**
    //
    // **見るのは全部の深さの最大である**——**深さごとに別の配列なので、
    // どれか 1 つが細っていれば危ない。**
    //
    // **カナリアは踏んでから言う。** **こちらは踏む前に言う。**
    let excursion_worst = serial
        .lines()
        .filter(|line| line.contains(" excursion stack ("))
        .filter_map(|line| line.split(" used ").nth(1))
        .filter_map(|rest| {
            let mut parts = rest.split(" of ");
            let used: usize = parts.next()?.trim().parse().ok()?;
            let capacity: usize = parts.next()?.split_whitespace().next()?.parse().ok()?;
            capacity.checked_sub(used)
        })
        .min();
    let excursion_stack_has_room =
        excursion_worst.is_some_and(|spare| spare >= EXCURSION_STACK_MIN_SPARE);

    // **`argv[0]` が打った語のままであること（3 本目）。**
    //
    // **`spawn-test beta` を `/bin/` を付けずに送っている。**
    // **`spawn-test` は `argv[0]` が "spawn-test" でなければ 3 で終わる**
    // （あちらの doc の終了状態の表）。**前置した `/bin/spawn-test` を
    // `argv[0]` にしていたら、長さの突き合わせで落ちて 3 になる。**
    //
    // **`Exited(0)` は 2 つを同時に主張する**——**解決したパスが
    // `/bin/spawn-test` であることと、渡した `argv[0]` が `spawn-test` である
    // ことである。**
    let argv0_is_as_typed = after_shell.contains("spawn: /bin/spawn-test ended (Exited(0))");

    println!("{context}: the shell exited with 0 = {ended}");
    println!("{context}: init started it again = {restarted}");
    println!("{context}: the shell was restarted exactly once = {restarted_only_once}");
    println!("{context}: the typed line was echoed = {echoed}");
    println!("{context}: ls listed the root = {ran_ls}");
    println!("{context}: cat printed /etc/motd = {ran_cat}");
    println!("{context}: hello ran = {ran_hello}");
    println!("{context}: the C program ran = {ran_c_hello}");
    println!("{context}: the C program took heap from brk = {c_heap_worked}");
    println!("{context}: bare names resolved under /bin = {bare_names_resolved}");
    println!("{context}: argv[0] stayed as typed = {argv0_is_as_typed}");
    println!("{context}: backspace edited the line = {backspace_edited_the_line}");
    println!(
        "{context}: the left arrow moved the insertion point = {moved} (wanted {})",
        mode.expects_the_cursor_to_move()
    );
    println!(
        "{context}: the literal Esc [ D keystrokes moved the insertion point = {esc_moved} \
         (wanted {})",
        mode.expects_esc_to_reach_ring3()
    );
    println!(
        "{context}: the two keys outside the table (ro, yen) reached ring 3 = \
         {jis_only_keys_reached_ring3} (only one of them arrived = {only_one_jis_key_arrived}; \
         wanted {})",
        mode.expects_the_jis_only_keys()
    );

    println!(
        "{context}: home and end moved the insertion point = \
         {home_and_end_moved_the_insertion_point}"
    );
    println!(
        "{context}: ctrl-a and ctrl-e moved the insertion point = \
         {ctrl_a_and_e_moved_the_insertion_point}"
    );
    println!(
        "{context}: delete removed the character = {delete_removed_the_character} (the cursor \
         moves = {})",
        mode.expects_the_cursor_to_move()
    );
    println!("{context}: tab stayed out of the line = {tab_stayed_out_of_the_line}");
    println!(
        "{context}: unknown control bytes stayed out of the line = \
         {unknown_ctrl_stayed_out_of_the_line}"
    );
    println!(
        "{context}: the history was walked with the arrows = {history_walked_with_arrows} \
         (ran aa {ran_aa} time(s); the cursor moves = {})",
        mode.expects_the_cursor_to_move()
    );
    println!("{context}: the history was walked with ctrl-p/n = {history_walked_with_ctrl}");
    println!("{context}: the full-length line was refused, not folded = {long_line_was_refused}");
    println!(
        "{context}: ctrl-b and ctrl-f moved the insertion point = \
         {ctrl_b_and_f_moved_the_insertion_point}"
    );
    println!(
        "{context}: the kernel stack kept at least {KERNEL_STACK_MIN_SPARE} byte(s) spare = \
         {kernel_stack_has_room} (spare {stack_spare:?})"
    );
    println!(
        "{context}: every excursion stack kept at least {EXCURSION_STACK_MIN_SPARE} byte(s) \
         spare = {excursion_stack_has_room} (worst spare {excursion_worst:?})"
    );
    println!("{context}: ctrl-k cut to the end = {ctrl_k_cut_to_the_end}");
    println!("{context}: ctrl-u cut to the start = {ctrl_u_cut_to_the_start}");
    println!("{context}: ctrl-w deleted the previous word = {ctrl_w_deleted_the_word}");
    println!("{context}: ctrl-d deleted the character = {ctrl_d_deleted_the_character}");
    println!(
        "{context}: ctrl-d on an empty line did nothing = {ctrl_d_on_an_empty_line_did_nothing}"
    );
    println!(
        "{context}: ctrl-l cleared and redrew (the screen itself is not observed here) = \
         {ctrl_l_redrew_the_screen}"
    );
    println!("{context}: the pending line came back = {the_pending_line_came_back}");
    println!(
        "{context}: duplicates were not stored = {duplicates_were_not_stored} (ran oo \
         {ran_oo} time(s))"
    );
    println!("{context}: the dollar stayed literal = {dollar_stayed_literal}");
    println!(
        "{context}: the keyboard layout in use is the expected one = {layout_is_in_use} \
         (expected {layout_output:?} from the two physical keys 0x1A and 0x27+Shift)"
    );
    println!(
        "{context}: ~ expands to HOME at the start of a word only = {tilde_expanded} \
         (~ = {tilde_alone}, ~/x = {tilde_with_path}, a~b left alone = {tilde_inside_a_word}; \
         wanted {})",
        mode.expects_the_tilde_to_expand()
    );
    println!(
        "{context}: every /bin/echo run in the script is named = {echo_lines_are_accounted_for} \
         (ran {} time(s), named {})",
        echo_argcs.len(),
        ECHO_LINES_IN_ORDER.len()
    );
    println!(
        "{context}: the exported name expanded = {expanded_the_exported_name} \
         (before/after argc = {:?}/{:?})",
        echo_argc("echo $ZF2 (before export)"),
        echo_argc("echo $ZF2 (after export)")
    );
    println!(
        "{context}: the child saw the exported name = {the_child_saw_the_exported_name} \
         (envc {envc_before_export:?} -> {envc_after_export:?}, wanted +{wanted_envc_growth})"
    );
    println!("{context}: set listed the table = {set_listed_the_table}");
    println!("{context}: export refused a bad name in the echo area = {export_refused_a_bad_name}");
    println!(
        "{context}: export changed the shell's own PATH = {export_changed_the_path} \
         (/bin/hello ran {hello_runs} time(s); the export script works = {})",
        mode.expects_the_export_script()
    );
    println!("{context}: echo $PATH printed the value = {expanded_a_value}");
    println!("{context}: the empty word was dropped = {empty_word_was_dropped}");
    println!("{context}: an unset name expanded to nothing = {expanded_to_nothing}");
    println!("{context}: the name expanded inside a word = {expanded_inside_a_word}");
    println!("{context}: ctrl-c discarded the half-typed line = {ctrl_c_discarded_the_line}");
    println!(
        "{context}: ctrl-c stopped the spinning child = {ctrl_c_stopped_the_child} (echoed ^C \
         count = {echoed_ctrl_c_count}, wanted 1)"
    );

    if ready
        && ended
        && restarted
        && echoed
        && ran_ls
        && ran_cat
        && ran_hello
        && ran_c_hello
        && c_heap_worked
        && bare_names_resolved
        && argv0_is_as_typed
        && backspace_edited_the_line
        && arrow_behaved_as_expected
        && esc_behaved_as_expected
        && (jis_only_keys_reached_ring3 == mode.expects_the_jis_only_keys())
        && !only_one_jis_key_arrived
        && home_and_end_moved_the_insertion_point
        && ctrl_a_and_e_moved_the_insertion_point
        && delete_removed_the_character
        && tab_stayed_out_of_the_line
        && unknown_ctrl_stayed_out_of_the_line
        && long_line_was_refused
        && history_walked_with_arrows
        && history_walked_with_ctrl
        && ctrl_b_and_f_moved_the_insertion_point
        && kernel_stack_has_room
        && excursion_stack_has_room
        && ctrl_k_cut_to_the_end
        && ctrl_u_cut_to_the_start
        && ctrl_w_deleted_the_word
        && ctrl_d_deleted_the_character
        && ctrl_d_on_an_empty_line_did_nothing
        && ctrl_l_redrew_the_screen
        && the_pending_line_came_back
        && duplicates_were_not_stored
        && dollar_stayed_literal
        && tilde_expanded
        && layout_is_in_use
        && echo_lines_are_accounted_for
        && expanded_the_exported_name
        && the_child_saw_the_exported_name
        && set_listed_the_table
        && export_refused_a_bad_name
        && export_changed_the_path
        && expanded_a_value
        && empty_word_was_dropped
        && expanded_to_nothing
        && expanded_inside_a_word
        && ctrl_c_discarded_the_line
        && ctrl_c_stopped_the_child
        && restarted_only_once
    {
        println!("{context}: PASS");
        if mode.expects_to_pass() {
            Ok(())
        } else {
            // **破壊が捕まらなかった。** 通ってしまったこと自体が失敗である。
            bail!("{context}: the sabotage was NOT caught; every judgement still held")
        }
    } else {
        println!("{context}: FAILED");
        if mode.expects_to_pass() {
            bail!("{context}: FAILED")
        } else {
            // **期待どおり落ちた。** どの判定が落ちたかは上に並んでいる。
            println!("{context}: the sabotage was caught (this run is expected to fail)");
            Ok(())
        }
    }
}

/// カーネルスタックに残っていてほしい余裕（P-c-1 の手当て）。
///
/// **16KiB である。** **根拠は実測**——**`kernel_main` を除くと、関数 1 つの枠は
/// 最大 4KiB である**（`objdump` でプロローグを走査した。2026-08-28）。
/// **4 つ積んでも足りる幅を取ってある。**
const KERNEL_STACK_MIN_SPARE: usize = 16 * 1024;

/// 遠征スタックに残っていてほしい余裕（f-2 の後の手当て）。
///
/// **[`KERNEL_STACK_MIN_SPARE`] と同じ 16KiB である。** **根拠も同じ**——
/// **関数 1 つの枠が最大 4KiB で、4 つ積んでも足りる幅である。**
///
/// # なぜ置くのか
///
/// **遠征スタックにはガードページが無い**（`.bss` の配列である。
/// `kernel/src/userland.rs` の `ring3:` の行）。**溢れは静かに起きて、
/// 下の静的領域を書く**——**実測で `EXCURSION_DEPTH` を壊したことがある**
/// （`docs/troubleshooting.md`）。
///
/// **底のカナリアは在るが、あれは踏んでから言う。** **踏む前に言うものが
/// 無かった**——**カーネルスタックで「緑であることと、余裕があることは違う」と
/// 書いたのと同じ形が、こちらに残っていた**（実測。2026-08-31。
/// **f-2 で深さ 0 の使用量が 40% から 43% へ動いたときに気づいた**）。
const EXCURSION_STACK_MIN_SPARE: usize = 16 * 1024;

/// `LINE_MAX` ちょうどの打鍵（SE-c）。**`z` を 128 個と Enter。**
///
/// **`kernel/userland/zash.rs` の `LINE_MAX` と同じ数である。**
/// **あちらを変えたらここも変えること**——**数が合わないと、溢れの境目を
/// 打たなくなる**（判定は静かに緑になる）。
const LONG_LINE_KEYS: usize = 128;

/// 上の打鍵を並べたもの。
static LONG_LINE_SCRIPT: [&str; LONG_LINE_KEYS + 1] = {
    let mut keys = ["z"; LONG_LINE_KEYS + 1];
    keys[LONG_LINE_KEYS] = "ret";
    keys
};

/// `--shell-test` が打つ行（S11-11。S12 前の手当ての 3 本目で伸ばした）。
///
/// **キー名は QEMU monitor のものである。** `/` は `slash`、空白は `spc`、
/// `-` は `minus`、改行は `ret` である。
///
/// # 順序に意味がある
///
/// **`/` を含む側を先に打つ。** あちらは 3 本目より前から通っていた道なので、
/// **固定の既定を入れて壊れていないことを先に見る。**
/// **そのあと `/` を含まない側を打つ。**
/// 台本の中で `/bin/echo` を起こす行を、**打つ順に**並べた名前（f-2）。
///
/// # なぜ名前の一覧を持つのか
///
/// **判定が `argc` を添字で引いていたためである。** **台本の途中へ `echo` の行を
/// 足すと添字がずれるが、ずれた先の値がたまたま同じなら緑のままになる**
/// ——**f-1 と f-1b で実際にそうなっていた**（実測。2026-08-31。
/// `Ctrl+W` と `$1` の 2 本が、`~` の行を見ていた）。
///
/// **ここに名前を並べ、本数を突き合わせる。** **台本へ `echo` の行を足したのに
/// ここへ足さなければ、`echo lines accounted for` が落ちる。**
///
/// **名前は打った行そのものにしてある**（編集の鍵を使う行だけ、何をしたかを添える）。
const ECHO_LINES_IN_ORDER: &[&str] = &[
    "echo $PATH",
    "echo a $UNSET b",
    "echo $UNSET",
    "echo a$TERM b",
    "echo ~",
    "echo ~/x",
    "echo a~b",
    "echo @+",
    // **f-2 で 2 本増えた。** **同じ行を 2 度打つ**——`export` の前と後である
    // （`ADR-0053` の判定 1 と 3）。**名前が同じなので、引くのは前のほうになる**
    // ——**後のほうは本数の差で見る**（下の `envc`）。
    "echo $ZF2 (before export)",
    "echo $ZF2 (after export)",
    "echo qq rr + Ctrl+W",
    "echo $1",
];

const SHELL_TEST_LINES: &[&[&str]] = &[
    // /bin/ls
    &["slash", "b", "i", "n", "slash", "l", "s", "ret"],
    // /bin/cat /etc/motd
    &[
        "slash", "b", "i", "n", "slash", "c", "a", "t", "spc", "slash", "e", "t", "c", "slash",
        "m", "o", "t", "d", "ret",
    ],
    // /bin/hello
    &[
        "slash", "b", "i", "n", "slash", "h", "e", "l", "l", "o", "ret",
    ],
    // /bin/chello（C-a。`ADR-0057`）。**C で書いたプログラムが走ること。**
    //
    // **`hello` の隣に置く。** **主張が同じ族だからである**——
    // **シェルが `/bin` の実行ファイルを起こせること。** **違うのは言語だけで、
    // 通る道（ELF ローダ・`int 0x80`・`spawn`）は同じである。**
    &[
        "slash", "b", "i", "n", "slash", "c", "h", "e", "l", "l", "o", "ret",
    ],
    // ls（`/` を含まない。`/bin/` の下で見つかること）
    &["l", "s", "ret"],
    // cat /etc/motd（`/` を含まない語 + `/` を含む引数）
    &[
        "c", "a", "t", "spc", "slash", "e", "t", "c", "slash", "m", "o", "t", "d", "ret",
    ],
    // spawn-test beta（`argv[0]` が打った語のままであること）
    &[
        "s", "p", "a", "w", "n", "minus", "t", "e", "s", "t", "spc", "b", "e", "t", "a", "ret",
    ],
    // abc → Backspace → x（S12 前の手当て）。**行編集が効いていることを見る。**
    //
    // **打つのは `abc`、消してから `x` なので、走るのは `abx` である。**
    // **消えていなければ `abcx` になる**——判定は 2 本で、
    // **出る側（`abx`）と出ない側（`abcx`）の両方を見る。**
    // どちらも実在しない語なので、シェルは `cannot run` を返す。
    &["a", "b", "c", "backspace", "x", "ret"],
    // pq → 左 → y（S12 前の手当て）。**挿入点が動いたことを見る。**
    //
    // **左へ 1 つ動いてから `y` を入れるので、走るのは `pyq` である。**
    // **動いていなければ `pqy` になる。** ここも 2 本で見る。
    &["p", "q", "left", "y", "ret"],
    // m → Esc → [ → D → n（zi-a）。**Esc が Ring 3 へ届いたことを見る。**
    //
    // **Esc キーそのものを打ち、`[` と `D` を続ける。** 届いていれば zash の
    // 状態機械が 3 打を `\x1b[D`（左）として解釈し、挿入点が 1 つ戻って
    // `n` が頭へ入る——**走るのは `nm` である。**
    // **届いていなければ Esc は捨てられ、`[` と `D` が字のまま入る**——
    // `m[Dn` になる。ここも 2 本で見る。
    // **`shift-d` は monitor のキー名である**（大文字 D。zash は `D` だけを
    // 左と解釈する）。
    //
    // **`bracket_right` で `[` を打つ（zi-e）。** **monitor のキー名は物理の
    // 位置を指しており、名前は US の刻印から付いている**——`bracket_left` は
    // `0x1A` で、**JIS ではそこが `@` である。** **既定を JIS にした段で、
    // 打つ位置を `0x1B`（JIS の `[`）へ移した。**
    //
    // **この 1 本が、実機で変換表を通る唯一の判定である**（台本の経路は
    // `read_bytes` へ直に差し込むのでデコーダを通らない。`kernel/src/input.rs`）。
    &["m", "esc", "bracket_right", "shift-d", "n", "ret"],
    // ろ → ¥ → Enter（zi-e）。**変換表の外に居る 2 キーが Ring 3 へ届くこと。**
    //
    // **どちらも `\` を出すので、走るのは `\\` である**（実在しない語なので
    // `cannot run` が返る）。**片方でも落ちれば語が `\` 1 文字になり、
    // 両方落ちれば空行になって何も走らない。** 3 つの結果が区別できる。
    //
    // **ここが見るのは前景の経路である**——`--interrupt-test keyboard` が
    // 見ているのはカーネル側の消費者（`drain_keyboard`）で、**消費者が違う。**
    // **同じ表を引くが、通る層が違うので両方に置く。**
    &["ro", "yen", "ret"],
    // **上下で行が壊れないことを見る台本は、SE-c で外した。**
    //
    // **あれは「zash が上下を読んで捨てる」ことに寄りかかっていた**（zi-a。
    // 履歴が無かった）。**SE-c で上下に意味ができたので、前提そのものが消えた。**
    // **上下が届くことは、下の履歴の判定が主張している。**
    // a → b → Home → c → End → d（SE-b。ADR-0050）。**Home と End が端へ動かす。**
    //
    // **走るのは `cabd` である**（`ab` の頭へ `c` を入れ、末尾へ `d` を入れる）。
    // **届いていなければ挿入点が動かず、`abcd` になる。** ここも 2 本で見る。
    &["a", "b", "home", "c", "end", "d", "ret"],
    // e → f → Ctrl+A → g → Ctrl+E → h（SE-b。ADR-0050）。**Ctrl+A と Ctrl+E。**
    //
    // **走るのは `gefh` である。** **一般化が外れていれば Ctrl+A は `a` を、
    // Ctrl+E は `e` を出す**ので、`efageh` になる。**その形も見る。**
    &["e", "f", "ctrl-a", "g", "ctrl-e", "h", "ret"],
    // i → j → 左 → Delete（SE-b）。**Delete が挿入点の字を消す。**
    //
    // **走るのは `i` である。** **扱っていなければ `3` と `~` が字として入り、
    // `i3~j` になる**——**実測でその形だった**（2026-08-28）。
    &["i", "j", "left", "delete", "ret"],
    // k → Tab → l（SE-b。ADR-0050）。**Tab が行へ入らない。**
    //
    // **走るのは `kl` である。** **捨てていなければ `0x09` が語に混ざる**
    // ——**実測でそうなっていた**（2026-08-28）。
    //
    // **Ctrl+D と別の行にしてある。** **落ちる破壊が違う**——
    // **Tab は Ctrl+英字ではないので、`keyboard-drop-ctrl-letters-test` では
    // 緑のままである。**
    &["k", "tab", "l", "ret"],
    // n → Ctrl+G → o（SE-b。ADR-0050。**SE-f で Ctrl+D から移した**）。
    // **一般化した Ctrl+英字のうち、受け手が決まっていないものが行へ入らない。**
    //
    // **走るのは `no` である。** **捨てていなければ `0x07` が語に混ざる。**
    // **以前は Ctrl+D を打っていたが、SE-f であれに意味ができた**
    // ——**受け手が決まった鍵では「捨てる」を主張できない。**
    // **`Ctrl+G` を選んだのは、受け手が無く、`0x0A`（Enter）や `0x09`（Tab）と
    // 重ならないためである。**
    // **`keyboard-drop-ctrl-letters-test` では Ctrl+G が `g` を出す**ので
    // `ngo` になり、この判定も落ちる。
    &["n", "ctrl-g", "o", "ret"],
    // `echo $PATH`（SE-d と SE-e。ADR-0049 と ADR-0043）。**展開が起きたこと。**
    //
    // **`$` は Shift+4 である**（JIS の表。`kernel/src/keyboard/decode.rs`）。
    // **出るのは `/bin` である。** **展開していなければ `$PATH` がそのまま出る。**
    &[
        "e", "c", "h", "o", "spc", "shift-4", "shift-p", "shift-a", "shift-t", "shift-h", "ret",
    ],
    // `echo a $UNSET b`（SE-d）。**丸ごと空になった語が落ちること。**
    //
    // **出るのは `a b` で、空白は 1 つである。** **落としていなければ
    // 空の語が `argv` に残り、`a  b` になる**（空白 2 つ）。
    // **この 1 本だけが `shell-keep-empty-word-test` を捕まえる。**
    &[
        "e", "c", "h", "o", "spc", "a", "spc", "shift-4", "shift-u", "shift-n", "shift-s",
        "shift-e", "shift-t", "spc", "b", "ret",
    ],
    // `echo $UNSET`（SE-d）。**語が 0 個になること。**
    //
    // **出るのは空行である。** **`echo` は引数が無くても改行を出す。**
    &[
        "e", "c", "h", "o", "spc", "shift-4", "shift-u", "shift-n", "shift-s", "shift-e",
        "shift-t", "ret",
    ],
    // `echo a$TERM b`（SE-d）。**`$` の直後以外の字が壊れないこと。**
    //
    // **出るのは `azaytos b` である**（`TERM` は `zaytos`。ADR-0041）。
    &[
        "e", "c", "h", "o", "spc", "a", "shift-4", "shift-t", "shift-e", "shift-r", "shift-m",
        "spc", "b", "ret",
    ],
    // `echo ~`（f-1。`ADR-0049` の Addendum）。**`~` が `HOME` へ展開されること。**
    //
    // **`~` は Shift+`^` である**（JIS の表。`0x0D`）。**monitor のキー名は
    // 物理の位置を指しており、名前は US の刻印から付いている**ので
    // `shift-equal` である（`bracket_right` と同じ事情）。
    //
    // **出るのは `/root` である**（`ADR-0052` の Decision 5）。
    // **展開していなければ `~` がそのまま出る。**
    &["e", "c", "h", "o", "spc", "shift-equal", "ret"],
    // `echo ~/x`（f-1）。**`~/` の形も展開されること。**
    //
    // **出るのは `/root/x` である。** **`~` 単独だけを見ると、
    // 「`~` で始まる語をすべて `HOME` に置き換える」形が通ってしまう。**
    &[
        "e",
        "c",
        "h",
        "o",
        "spc",
        "shift-equal",
        "slash",
        "x",
        "ret",
    ],
    // `echo a~b`（f-1。Addendum の規則 1）。**語の先頭でなければ字である。**
    //
    // **出るのは `a~b` そのものである。** **こちらが主張の主である**
    // ——**展開する側だけを見ると、どこでも展開する形が通る。**
    &["e", "c", "h", "o", "spc", "a", "shift-equal", "b", "ret"],
    // `echo @+`（f-1b）。**変換表が実行時に選ばれていることを見る。**
    //
    // **物理キーを 2 つ打つ。** **`bracket_left` は `0x1A`、
    // `shift-semicolon` は `0x27` の Shift である**（monitor のキー名は
    // 物理の位置を指しており、名前は US の刻印から付いている）。
    //
    // **JIS では `@` と `+`、US では `[` と `:` が出る**（実測。
    // `kernel/src/keyboard/decode.rs` の 2 つの表を突き合わせた）。
    //
    // **2 つに分けてあるのは、`character_for` の別の経路を通るためである**
    // ——**`0x1A` は素の表、`0x27` は Shift の表から来る。**
    // **片方だけ切り替わる形を捕まえる。**
    &[
        "e",
        "c",
        "h",
        "o",
        "spc",
        "bracket_left",
        "shift-semicolon",
        "ret",
    ],
    // --- f-2（`export` と `set`。`ADR-0053`）。**ここから下は末尾に足した** ---
    //
    // **末尾に置く理由は 2 つある。** **`echo` の `argc` の並びを見ている判定が
    // 前に在り、間へ入れると添字がずれる**（族「台本を変えるときは、台本に
    // 寄りかかっている判定を数え直すこと」）。**そして `PATH` を壊す行が在るので、
    // 後続の行に影響を出さない位置に置く。**
    //
    // `echo $ZF2`（打つ前）。**まだ置いていないので、語が 0 個になり空行が出る。**
    // **`envc` は 3 である**——**「打つ前」を見る側である**（`ADR-0053` の判定 3）。
    &[
        "e", "c", "h", "o", "spc", "shift-4", "shift-z", "shift-f", "2", "ret",
    ],
    // `export ZF2=exported`。**`=` は JIS では `-` キーの Shift である。**
    &[
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "spc",
        "shift-z",
        "shift-f",
        "2",
        "shift-minus",
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "e",
        "d",
        "ret",
    ],
    // `echo $ZF2`（打った後）。**シェルの表から引けること**（判定 1）。
    // **`envc` は 4 になる**——**子へ届いたこと**（判定 3）。
    &[
        "e", "c", "h", "o", "spc", "shift-4", "shift-z", "shift-f", "2", "ret",
    ],
    // `set`。**表の一覧が出ること**（判定 2）。
    &["s", "e", "t", "ret"],
    // `export 1BAD=x`。**誤りが人に見えること**（判定 5。運用者の指示）。
    //
    // **名前の規則で断られる**——`[A-Za-z_]` で始まらない。
    &[
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "spc",
        "1",
        "shift-b",
        "shift-a",
        "shift-d",
        "shift-minus",
        "x",
        "ret",
    ],
    // `export PATH=/nope` → `hello` → `export PATH=/bin` → `hello`（判定 4）。
    //
    // **シェル自身の振る舞いが変わること**（`ADR-0053` の Decision 6。
    // **控えたままだと「変えたのに効かない」が残る**）。
    //
    // **`ls` ではなく `hello` を使う。** **`bare names resolved under /bin` が
    // 「`zash: ls: cannot run` が出ないこと」を見ており、`ls` を落とすと
    // あちらが壊れる**（族「台本を変えるときは、台本に寄りかかっている判定を
    // 数え直すこと」。**数え直して見つけた**）。
    &[
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "spc",
        "shift-p",
        "shift-a",
        "shift-t",
        "shift-h",
        "shift-minus",
        "slash",
        "n",
        "o",
        "p",
        "e",
        "ret",
    ],
    &["h", "e", "l", "l", "o", "ret"],
    &[
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "spc",
        "shift-p",
        "shift-a",
        "shift-t",
        "shift-h",
        "shift-minus",
        "slash",
        "b",
        "i",
        "n",
        "ret",
    ],
    &["h", "e", "l", "l", "o", "ret"],
    // aa / bb を打ってから 上 上（SE-c）。**履歴を矢印で辿る。**
    //
    // **辿れていれば `aa` が 2 度走る**（打ったときと、辿って Enter したとき）。
    // **積んでいなければ上は何もせず、空行になって 1 度きりである。**
    // **数で見る**——同じ語なので、出る側と出ない側では分けられない。
    &["a", "a", "ret"],
    &["b", "b", "ret"],
    &["up", "up", "ret"],
    // cc / dd を打ってから Ctrl+P Ctrl+P Ctrl+N（SE-c）。**同じ履歴を Ctrl で辿る。**
    //
    // **2 つ戻って 1 つ進むので、走るのは `dd` である**（2 度目）。
    // **矢印と同じ関数を通しているが、それは判定になっていない**ので、
    // **両方の経路に判定を置く。**
    &["c", "c", "ret"],
    &["d", "d", "ret"],
    &["ctrl-p", "ctrl-p", "ctrl-n", "ret"],
    // r s → Ctrl+B → t → Ctrl+F → u（SE-c）。**Ctrl+B と Ctrl+F が左右へ動かす。**
    //
    // **走るのは `rtsu` である。** **効いていなければ `b` と `f` が字として入り、
    // `rsbtfu` になる**（`keyboard-drop-ctrl-letters-test` がその形である）。
    &["r", "s", "ctrl-b", "t", "ctrl-f", "u", "ret"],
    // `LINE_MAX` ちょうどの行（SE-c。`d7de0ce` の修正に判定を置く）。
    //
    // **`z` を 128 打ってから Enter する。** **入るのは 127 までで、128 打目は
    // 溢れとして捨てられる**ので、**出るのは `zash: line too long` である。**
    //
    // **直す前はここで畳まれていた**——**`length` が `LINE_MAX` になり、
    // `line[length] = 0` が配列の外を書いた。** **畳まれるとシェルが止まるので、
    // この行より後ろの判定が全部落ちる。**
    //
    // **費用は打鍵の数である**（実測。この 1 行で `--shell-test` が 69.7 秒から
    // 86.0 秒へ延びた。**`--full` は 14 回走らせるので約 3.8 分増える**）。
    &LONG_LINE_SCRIPT,
    // echo qq rr → Ctrl+W（SE-f）。**直前の語を消す。**
    //
    // **切れ目は空白だけである**（`zash` の `word_start`）。**`qq rr` の末尾で
    // 打つと `qq ` が残るので、出るのは `qq` で `argc` は 2 である。**
    // **効いていなければ `w` が字として入り、`qq rrw` になる**（`argc` は 3）。
    &[
        "e", "c", "h", "o", "spc", "q", "q", "spc", "r", "r", "ctrl-w", "ret",
    ],
    // echo $1（SE-f。`ADR-0049` の 5）。**`$` の直後が名前の先頭でなければ字である。**
    //
    // **出るのは `$1` そのものである。** **展開していれば消えるか、別の値になる。**
    &["e", "c", "h", "o", "spc", "shift-4", "1", "ret"],
    // kkxx → Ctrl+A → Ctrl+F Ctrl+F → Ctrl+K（SE-f）。**挿入点から行末まで消す。**
    //
    // **走るのは `kk` である。** **効いていなければ `kkxx` のままである。**
    &[
        "k", "k", "x", "x", "ctrl-a", "ctrl-f", "ctrl-f", "ctrl-k", "ret",
    ],
    // uuyy → Ctrl+A → Ctrl+F Ctrl+F → Ctrl+U（SE-f）。**行頭から挿入点まで消す。**
    //
    // **走るのは `yy` である。** **効いていなければ `uuyy` のままである。**
    // **`Ctrl+K` と範囲の計算が逆なので、別の 1 本にする。**
    &[
        "u", "u", "y", "y", "ctrl-a", "ctrl-f", "ctrl-f", "ctrl-u", "ret",
    ],
    // mxmy → Ctrl+A → Ctrl+F → Ctrl+D（SE-f）。**挿入点の字を消す。**
    //
    // **走るのは `mmy` である。** **効いていなければ `mxmy` のままである。**
    &["m", "x", "m", "y", "ctrl-a", "ctrl-f", "ctrl-d", "ret"],
    // 空行で Ctrl+D → ee（SE-f）。**空行では何もしない。**
    //
    // **`bash` はここで EOF になりシェルが終わる。** **ZaytOS は何もしない**ので、
    // **続けて打った `ee` がそのまま走る。** **終わっていれば `init` が起こし直し、
    // 「ちょうど 1 回」の判定が落ちる。**
    &["ctrl-d", "e", "e", "ret"],
    // ll → Ctrl+L（SE-f）。**画面を消して描き直す。行は消さない。**
    //
    // **走るのは `ll` である。** **消す並びが出ていることは別に見る。**
    &["l", "l", "ctrl-l", "ret"],
    // st → 上 → Ctrl+N（SE-c の宿題）。**打ちかけの行が戻ること。**
    //
    // **辿ってから戻ると、打ちかけだった `st` が復る。** **復らなければ、
    // 辿った先の行か空行になる。**
    // **矢印を落とす構成でも `st` のままである**（上が届かないので辿らない）
    // ——**どちらの構成でも同じ主張になる。**
    &["s", "t", "up", "ctrl-n", "ret"],
    // oo / pp / pp → 上 上（SE-c の宿題）。**同じ行が 2 つ並ばないこと。**
    //
    // **積まなければ履歴は `oo` `pp` の 2 本で、上 上 は `oo` へ届く。**
    // **並べば `pp` `pp` になり、上 上 は `pp` で止まる。**
    &["o", "o", "ret"],
    &["p", "p", "ret"],
    &["p", "p", "ret"],
    &["up", "up", "ret"],
    // 打ちかけの行を Ctrl+C で捨てる（S12 前の手当て、C）。**深さ 1 の側である。**
    //
    // **`zz` と打ってから Ctrl+C を送り、そのまま `ret` を打つ。**
    // **捨てられていれば空行なので何も走らない。**
    // **捨てられていなければ `zz` が走り、`cannot run` が出る。**
    //
    // **ここだけ右 Ctrl を使う**（`ctrl_r`。深さ 2 の側は左 Ctrl のままである）。
    // **左右で同じに扱うと決めた**——右は `0xE0 0x1D` で来るので、接頭辞を
    // 読み捨てないと左と区別が付く。**決めたことを回帰で守る。**
    // **項目は増えない**——同じ 1 本の中で、左右の両方が通る形にしてある。
    &["z", "z", "ctrl_r-c", "ret"],
    // 回り続ける子を Ctrl+C で止める（S12 前の手当て、C）。**深さ 2 の側である。**
    //
    // **`spin` はシステムコールを出さずに回る**ので、**止められなければ
    // ここで永久に止まる**（`--shell-test` は待ち時間の上限で落ちる）。
    //
    // **`ret` の後に間が要る。** 子が起きて 1 行出すまで待ってから Ctrl+C を送る
    // ——**起きる前に送ると、旗が子より先に立って `spawn` が降ろしてしまう。**
    &["s", "p", "i", "n", "ret"],
    &["ctrl-c"],
    // exit
    &["e", "x", "i", "t", "ret"],
];

fn cmd_keyboard_test() -> Result<()> {
    let assertions = run_keyboard_test(&[])?;
    if assertions.all_ok() {
        println!("interrupt-test keyboard: OK");
        Ok(())
    } else {
        bail!("interrupt-test keyboard: FAILED")
    }
}

fn run_keyboard_test(features: &[&str]) -> Result<KeyboardAssertions> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("keyboard-test-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);
    // **screenshot と同じく /tmp の短いパスを使う。** workspace 配下に置くと、
    // 作業ディレクトリが深い場所（git worktree を /tmp の下に作った場合など）で
    // `sun_path` の 108 バイト上限を超える。超えると QEMU はソケットを作れずに
    // 即座に終了し、ゲストの出力が 1 行も出ない。症状が起動失敗と区別できず、
    // 実際にこれを「OVMF の起動フレーキネス」と 8 回連続で誤認しかけた。
    let monitor_socket = PathBuf::from(format!(
        "/tmp/zaytos-xtask-keyboard-{}.sock",
        std::process::id()
    ));
    let _ = fs::remove_file(&monitor_socket);
    ensure_socket_path_fits(&monitor_socket)?;

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: Some(&monitor_socket),
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the keyboard test")?;

    // キーを送る前に、IRQ1 が解禁されるまで待つ。**上限つき。**
    let ready_marker = "keyboard: IRQ1 is unmasked";
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let mut ready = false;
    while Instant::now() < deadline {
        if read_lossy(&serial_log).contains(ready_marker) {
            ready = true;
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let mut expected_codes = 0u64;
    if ready {
        match connect_monitor_with_retry(&monitor_socket) {
            Ok(mut stream) => {
                for key in KEYBOARD_TEST_KEYS {
                    if writeln!(stream, "sendkey {}", key.monitor).is_err() {
                        break;
                    }
                    expected_codes += key.codes;
                    thread::sleep(Duration::from_millis(200));
                }
            }
            Err(e) => println!("keyboard-test: could not reach the QEMU monitor: {e}"),
        }
        // 反映とハートビートの更新を待つ。
        thread::sleep(Duration::from_secs(3));
    }

    // **kill する前に、既に終わっていないかを見る。** 自分から終了して
    // いたなら、それは QEMU 側の異常（引数が不正、OVMF が無い、ソケットを
    // 作れない等）であって、ゲストが動かなかったのとは別である。この 1 行が
    // 無いと両者を区別できない。
    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&monitor_socket);

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);

    let context = "interrupt-test keyboard";
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        // 上は必ず Err を返すので、ここには来ない。
        bail!("{context}: the kernel did not start");
    }

    println!("--- {context}: relevant output ---");
    for line in serial
        .lines()
        .filter(|l| l.contains("keyboard") || l.contains("heartbeat") || l.contains("i8042"))
    {
        println!("{line}");
    }
    println!("--- end ---");

    let mut ok = ready;
    if !ready {
        println!("{context}: the kernel never reached {ready_marker:?} = NG");
    }
    let ready_ok = ready;

    // 1. 期待した文字列になったか（大文字と記号の変換を含む）。
    let line_ok = serial.contains(KEYBOARD_TEST_EXPECTED_LINE);
    println!(
        "{context}: serial contains {KEYBOARD_TEST_EXPECTED_LINE:?} = {}",
        if line_ok { "OK" } else { "NG" }
    );
    ok &= line_ok;

    // 2. IRQ1 の配送経路（**到達**）。S2-d-1c で 0x21 から 0x42 へ変えた。
    //
    // **0x42 は 8259 が出しえないベクタである。** ベクタオフセットは
    // 0x20（alt-offset では 0x30）で、そこに IRQ 番号を足したものしか出ない。
    // したがって**このベクタで届いたこと自体が、I/O APIC 経由である証拠**になる。
    // 0x21 のままだとどちらの経路でも同じ値になり、何も示さなかった。
    let vector_ok = serial.contains("keyboard: first key arrived as vector 0x42");
    println!(
        "{context}: the first key arrived as vector 0x42, which the 8259 cannot produce = {}",
        if vector_ok { "OK" } else { "NG" }
    );
    ok &= vector_ok;

    // 2b. 二重配送が起きていないこと（PPP'）。
    //
    // 移行後は経路ごとにベクタが違うので、旧ベクタで届いたキーがあれば
    // **PIC 側の線が開いたままである。** 移行前は区別できなかった形である。
    let no_legacy_delivery = !serial.contains("keyboard: first key arrived as vector 0x21");
    println!(
        "{context}: no key arrived on the old 8259 vector 0x21 = {}",
        if no_legacy_delivery { "OK" } else { "NG" }
    );
    ok &= no_legacy_delivery;

    // 2c. 設定の読み戻し（**到達とは独立した検出経路**）。
    //
    // 読み戻しだけだと、設定できても配送されない形（マスクの外し忘れ、
    // 宛先の誤り）を通す。到達だけだと、書いた値が entry に保持されて
    // いるかを見ていない。**両方あって初めて経路が閉じる。**
    //
    // **ベクタ欄とマスクビットを別々に見る。** 1 つに畳むと、マスクを外し
    // 忘れた構成で「設定が書けていない」と読めてしまい、2 つの経路が
    // 独立していることを示せない（実際に畳んだ形で測って気づいた）。
    let readback_line = serial
        .lines()
        .find(|l| l.contains("ioapic: IRQ1 redirection entry read back:"))
        .unwrap_or_default();
    let readback_vector_ok = readback_line.contains("vector=0x42");
    println!(
        "{context}: the redirection entry carries the vector we wrote = {}",
        if readback_vector_ok { "OK" } else { "NG" }
    );
    ok &= readback_vector_ok;

    let readback_unmasked_ok = readback_line.contains("masked=false");
    println!(
        "{context}: the redirection entry is unmasked = {}",
        if readback_unmasked_ok { "OK" } else { "NG" }
    );
    ok &= readback_unmasked_ok;

    // 3. 送った本数と受け取った本数の一致（取りこぼしなし）。
    let expected_keys = format!("keys={expected_codes} ");
    let count_ok = serial.contains(&expected_keys);
    println!(
        "{context}: received exactly {expected_codes} scancode(s) = {}",
        if count_ok { "OK" } else { "NG" }
    );
    ok &= count_ok;

    // 4. 会計が閉じていること、溢れていないこと。
    let balanced_ok = serial.contains("balanced=true") && !serial.contains("balanced=false");
    println!(
        "{context}: the scancode accounting balances = {}",
        if balanced_ok { "OK" } else { "NG" }
    );
    ok &= balanced_ok;

    // 「dropped= が出ていて、そのすべてが 0」であることを見る。
    // contains("dropped=0") だけだと、別の行に dropped=3 があっても通る。
    let no_drop_ok = serial.contains("dropped=0")
        && !serial
            .lines()
            .any(|l| l.contains("dropped=") && !l.contains("dropped=0 "));
    println!(
        "{context}: no scancode was dropped = {}",
        if no_drop_ok { "OK" } else { "NG" }
    );
    ok &= no_drop_ok;

    // 5. ティックが進み続けていること（タイマとキーボードの共存）。
    let heartbeats = serial.matches("heartbeat: ticks=").count();
    println!("{context}: heartbeat lines = {heartbeats} (expected at least 2)");
    ok &= heartbeats >= 2;

    // 6. 意図しない例外が起きていないこと。
    //
    // S8-d/e の遠征が Ring 3 の #PF を意図して起こす（未マップ VA とカーネル VA の
    // 2 本）。除外は CR2 の列挙ではなく **cpl=3 で行う**——S8 以降、Ring 3 由来の
    // #PF は畳まれて処理される事象であり、この検査が守るべき不変条件は
    // 「意図しない**カーネルの** #PF が無いこと」だからである。CR2 を並べる形だと
    // 遠征を足すたびに除外が増え、増えた分だけ検査が守る範囲が黙って狭くなる。
    // cpl=0 の #PF は従来どおり NG、#DF（v=08）も無条件に NG である。
    let unintended_pf = qemu
        .lines()
        .any(|l| l.contains("v=0e") && !l.contains("cpl=3"));
    println!(
        "{context}: qemu log free of ring-0 \"v=0e\" = {}",
        if unintended_pf { "NG" } else { "OK" }
    );
    ok &= !unintended_pf;
    let df_present = qemu.contains("v=08");
    println!(
        "{context}: qemu log free of \"v=08\" = {}",
        if df_present { "NG" } else { "OK" }
    );
    ok &= !df_present;

    // ready / no_drop / heartbeats / 例外の 4 つは、どの構成でも成り立つべき
    // 前提として `line` へ畳んでいる。破壊確認が見分けたいのは、
    // 到達・二重配送・読み戻し・本数の 4 つである。
    let assertions = KeyboardAssertions {
        line: ready_ok && line_ok && no_drop_ok && heartbeats >= 2 && ok,
        arrived_on_new_vector: vector_ok,
        no_legacy_delivery,
        readback_vector: readback_vector_ok,
        readback_unmasked: readback_unmasked_ok,
        count: count_ok,
        balanced: balanced_ok,
        refused_sti: serial.contains("refusing to sti"),
    };
    println!(
        "{context}: {}",
        if assertions.all_ok() { "PASS" } else { "FAIL" }
    );
    println!(
        "{context}: note - key repeat (typematic) is NOT covered here; QEMU's sendkey does \
         not emulate it. Check it by hand with `cargo xtask run --gui --manual`."
    );
    Ok(assertions)
}

/// S2-d-2 の検査。**ホストの実時間を独立の基準として使う。**
///
/// カーネル側の許容幅の判定は「要求周波数」と「較正値から導いた実効周波数」を
/// 比べており、**どちらも内側の値である。** 較正値が 2 倍になれば初期カウントも
/// 2 倍になって比は変わらず、**自己無矛盾のまま通ってしまう。**
///
/// ここではハートビートが報告する経過秒を、**ホスト側で測った実時間**と
/// 突き合わせる。較正値が現実とずれていれば、この比がずれる。
struct LapicTimerTest {
    name: &'static str,
    /// 追加する feature。既定ビルドなら空。
    features: &'static [&'static str],
    /// 実時間との比がこの範囲に入るべきか。
    expect_within_tolerance: bool,
    /// 破壊が適用されたことを示すマーカー（空なら見ない）。
    ///
    /// **破壊のフックが踏まれなかった場合、破壊ビルドは正常に見えて緑になる。**
    /// 適用の痕跡を必須にして、空振りを落とす。
    sabotage_marker: &'static str,
    /// 空でなければ、**カーネルがこの行を出して停止することを期待する。**
    /// 速さの比ではなく、名指しの検出で捕まる破壊に使う。
    expect_halt_marker: &'static str,
}

const LAPIC_TIMER_TESTS: &[LapicTimerTest] = &[
    LapicTimerTest {
        name: "rate",
        // **シェルへ渡さない構成で測る（S11-11）。** 20 秒ぶんのティックが要る。
        features: &["keep-steady-loop"],
        expect_within_tolerance: true,
        sabotage_marker: "",
        expect_halt_marker: "",
    },
    // 較正の戻り値を 2 倍にする。**カーネル内の比は変わらないが、実時間との
    // 比は倍になる。** これが「較正値が初期カウントへ実際に流れている」ことの
    // 証明である。
    LapicTimerTest {
        name: "scaled-calibration",
        features: &["lapic-timer-scale-calibration-test"],
        expect_within_tolerance: false,
        sabotage_marker: "apic: SABOTAGE applied - the calibration result is scaled",
        expect_halt_marker: "",
    },
    // 較正で書く分周と、戻り値に載せる分周を食い違わせる。**較正は分周なしで
    // 測るので周波数が 16 倍に出て、運用は 16 分周で走る。** 実時間との比が
    // 大きく崩れる。
    //
    // **検出経路は `scaled-calibration` と同じ（実時間との比）である。**
    // 主張は違う（あちらは較正値が初期カウントへ流れること、こちらは較正時と
    // 運用時で分周が揃うこと）が、**落ちる場所は同じ**である。
    LapicTimerTest {
        name: "wrong-divide",
        features: &["lapic-timer-wrong-divide-test"],
        expect_within_tolerance: false,
        sabotage_marker: "",
        expect_halt_marker: "",
    },
    // PIC を全マスクせずに LVT を開ける。**速さの比では捕まらない。**
    // 切り替えの直後に IMR を読み戻す検査が、IRQ0 が開いたままであることを
    // 名指しで捕まえて停止する。**`irq::mask_all()` が実際に呼ばれている
    // ことの裏返しの証明でもある**（呼ばれていなければこの検査が落ちる）。
    LapicTimerTest {
        name: "no-mask-all",
        features: &["lapic-timer-no-mask-all-test"],
        expect_within_tolerance: false,
        sabotage_marker: "",
        expect_halt_marker: "lapic-timer: the 8259 is not fully masked",
    },
];

/// ハートビートが報告する経過秒と、ホストで測った実時間の比を見る。
///
/// 許容幅はカーネル側の ±0.5% より**大きく取る**。QEMU の実行そのものが
/// 実時間に対して一定の比で進む保証が無く、起動処理やホストの負荷も混ざる
/// ためである。**ここで見たいのは「桁が合っているか」であって、
/// 較正の精度ではない。**
const LAPIC_TIMER_RATE_TOLERANCE: f64 = 0.25;

fn cmd_lapic_timer_test(kind: &str) -> Result<()> {
    let test = LAPIC_TIMER_TESTS
        .iter()
        .find(|t| t.name == kind)
        .with_context(|| {
            let names: Vec<&str> = LAPIC_TIMER_TESTS.iter().map(|t| t.name).collect();
            format!(
                "unknown --lapic-timer-test kind: {kind} ({})",
                names.join(" | ")
            )
        })?;
    let context = format!("lapic-timer-test {}", test.name);

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, test.features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root.join("target").join("lapic-timer-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the lapic timer test")?;

    // **最初のハートビートが出てから測り始める。** 起動処理の時間を
    // 分母に入れると、比が起動の重さに引きずられる。
    let first_marker = "heartbeat: ticks=";
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let mut started_at = None;
    let mut first_seconds = 0u64;
    while Instant::now() < deadline {
        if let Some((seconds, _)) = last_heartbeat_seconds(&serial_log) {
            started_at = Some(Instant::now());
            first_seconds = seconds;
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let mut measured = None;
    if let Some(started_at) = started_at {
        thread::sleep(LAPIC_TIMER_MEASURE_WINDOW);
        if let Some((seconds, _)) = last_heartbeat_seconds(&serial_log) {
            measured = Some((
                seconds.saturating_sub(first_seconds),
                started_at.elapsed().as_secs_f64(),
            ));
        }
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip, qemu_exit.as_deref());
    }

    let mut ok = true;

    // 破壊が実際に適用されたこと。**空振りを落とす。**
    if !test.sabotage_marker.is_empty() {
        let applied = serial.contains(test.sabotage_marker);
        println!(
            "{context}: the sabotage was actually applied = {}",
            if applied { "OK" } else { "NG" }
        );
        if !applied {
            println!("{context}: the sabotage hook was never reached, so this run proves nothing");
        }
        ok &= applied;
    }

    // 名指しの検出で捕まる破壊は、速さの比を見ない。
    if !test.expect_halt_marker.is_empty() {
        let halted = serial.contains(test.expect_halt_marker);
        println!(
            "{context}: the kernel refused to proceed with the named check = {}",
            if halted { "OK" } else { "NG" }
        );
        ok &= halted;
        if ok {
            println!("{context}: PASS");
            return Ok(());
        }
        bail!("{context}: FAIL")
    }

    let ticking = serial.contains(first_marker);

    match measured {
        Some((kernel_seconds, host_seconds)) if host_seconds > 0.0 => {
            println!(
                "{context}: heartbeats are being produced = {}",
                if ticking { "OK" } else { "NG" }
            );
            ok &= ticking;

            let ratio = kernel_seconds as f64 / host_seconds;
            let within = (ratio - 1.0).abs() <= LAPIC_TIMER_RATE_TOLERANCE;
            println!(
                "{context}: kernel {kernel_seconds} s vs host {host_seconds:.1} s (ratio {ratio:.3}), within {LAPIC_TIMER_RATE_TOLERANCE} = {within} (wanted {})",
                test.expect_within_tolerance
            );
            ok &= within == test.expect_within_tolerance;
        }
        // **ハートビートが 1 本も出ないのも「比が崩れている」の一形態である。**
        // 遅くなる向きに壊すと、測定窓の中に 1 本も入らない。**「測れなかった」
        // で片づけると、壊れていることを検出できたのに落としてしまう。**
        //
        // ただし**正常であるはずの構成では失敗として扱う。** 出ないことが
        // 正しいのは、幅の外へ出ると宣言した構成だけである。
        _ if !test.expect_within_tolerance => {
            println!(
                "{context}: no heartbeat appeared within the window, which is itself out of tolerance (the timer is far too slow) = OK"
            );
            // 起動そのものが失敗した場合と区別する。
            let started = serial.contains("lapic-timer: the timer now arrives as vector");
            println!(
                "{context}: the switch to the local APIC timer did happen = {}",
                if started { "OK" } else { "NG" }
            );
            ok &= started;
        }
        _ => {
            println!("{context}: could not measure the tick rate = NG");
            ok = false;
        }
    }

    if ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAIL")
    }
}

/// 測定窓。**短いとハートビートの粒度（1 秒）が効きすぎる。**
const LAPIC_TIMER_MEASURE_WINDOW: Duration = Duration::from_secs(20);

/// シリアルログの最後のハートビートが報告する経過秒とティック数。
fn last_heartbeat_seconds(serial_log: &Path) -> Option<(u64, u64)> {
    let content = read_lossy(serial_log);
    let line = content.lines().rfind(|l| l.contains("heartbeat: ticks="))?;
    // 形は `heartbeat: ticks=256 (2 s), ...`
    let ticks_part = line.split("ticks=").nth(1)?;
    let ticks: u64 = ticks_part.split_whitespace().next()?.parse().ok()?;
    let seconds_part = line.split('(').nth(1)?;
    let seconds: u64 = seconds_part.split_whitespace().next()?.parse().ok()?;
    Some((seconds, ticks))
}

/// AP のハートビートが報告するティック数（S4-a）。
fn last_ap_heartbeat_ticks(serial_log: &Path) -> Option<u64> {
    let content = read_lossy(serial_log);
    let line = content
        .lines()
        .rfind(|l| l.contains("smp: ap heartbeat: cpu=1 ticks="))?;
    // 形は `smp: ap heartbeat: cpu=1 ticks=8400 tsc=...`
    let ticks_part = line.split("ticks=").nth(1)?;
    ticks_part.split_whitespace().next()?.parse().ok()
}

/// AP のティックのレートをホストの実時間と突き合わせる（S4-a）。
///
/// # これが検証しているのは仮定である
///
/// AP は較正をやり直さず、**BSP が測った分周と初期カウントをそのまま自分の LVT へ
/// 書く。** それは「Local APIC タイマの周波数がコア間で同じ」という**仮定**である。
///
/// **カーネルの内側では確かめられない。** 要求周波数も実効周波数も同じ較正値から
/// 導くので自己無矛盾になる（`lapic-timer-scale-calibration-test` が示した形と
/// 同じである）。**独立な基準はホストの実時間しかない。**
///
/// 仮定が崩れる環境（コアごとに LAPIC タイマの周波数が違う機械）では、
/// **AP のティックのレートがホスト時間と合わなくなり、ここで捕まる。**
fn cmd_ap_timer_rate() -> Result<()> {
    let context = "smp-ap-test ap-timer-rate";
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, &["keep-steady-loop"])?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root.join("target").join("smp-ap-rate-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });
    qemu_args.push("-smp".into());
    qemu_args.push("2".into());

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the AP timer rate test")?;

    // **AP の最初のハートビートが出てから測り始める。** 起動処理の時間を
    // 分母に入れると、比が起動の重さに引きずられる（BSP 側の測り方と同じ）。
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let mut started_at = None;
    let mut first_ticks = 0u64;
    while Instant::now() < deadline {
        if let Some(ticks) = last_ap_heartbeat_ticks(&serial_log) {
            started_at = Some(Instant::now());
            first_ticks = ticks;
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let mut measured = None;
    if let Some(started_at) = started_at {
        thread::sleep(LAPIC_TIMER_MEASURE_WINDOW);
        if let Some(ticks) = last_ap_heartbeat_ticks(&serial_log) {
            measured = Some((
                ticks.saturating_sub(first_ticks),
                started_at.elapsed().as_secs_f64(),
            ));
        }
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(context, firmware_rip, qemu_exit.as_deref());
    }

    let mut ok = true;
    let Some((ap_ticks, host_seconds)) = measured else {
        println!("{context}: could not measure the AP tick rate = NG");
        bail!("{context}: FAIL")
    };
    if host_seconds <= 0.0 {
        println!("{context}: the measurement window was empty = NG");
        bail!("{context}: FAIL")
    }

    // **要求周波数はカーネルの外へ出ていない。** ハートビートの粒度から導く。
    // AP のハートビートは 100 ティックごとなので、ティック数そのものを使う。
    let measured_hz = ap_ticks as f64 / host_seconds;
    let ratio = measured_hz / AP_EXPECTED_TICK_HZ;
    let within = (ratio - 1.0).abs() <= LAPIC_TIMER_RATE_TOLERANCE;
    println!(
        "{context}: AP produced {ap_ticks} tick(s) in {host_seconds:.1} host second(s) = \
         {measured_hz:.2} Hz, expected {AP_EXPECTED_TICK_HZ:.0} Hz (ratio {ratio:.3}), \
         within {LAPIC_TIMER_RATE_TOLERANCE} = {within}"
    );
    println!(
        "{context}: this is the check that the shared calibration is a valid ASSUMPTION; the \
         kernel cannot check it from inside because both sides come from the same calibration"
    );
    ok &= within;

    if ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAIL")
    }
}

/// カーネル入口への同時進入を実測する（S4-a）。**KVM でしか観測できない。**
///
/// # なぜ TCG では駄目なのか
///
/// **TCG は 2 つの vCPU を並行に走らせない**（実測）。`-smp 2` を 86 秒
/// （17,060 ティック）回しても同時進入数は 1 のままで、入口の窓を 3,000 回の
/// `spin_loop` ぶん意図的に広げても 0 回だった。**「稀」ではなく「起きない」である。**
/// KVM では 2 になる。
///
/// # **検査項目ではない。手動で回す観測である**
///
/// **KVM でも決定的ではない。** 同じ構成で 4 回回して 3 回は 2 が出たが、
/// 1 回は 116 秒（両コアで約 23,000 回の入口通過）のあいだ 1 のままだった。
/// ホスト側のスケジューリング次第で重なるかどうかが変わる。
/// **確率的なものを `--full` に入れると、落ちたときに退行か揺らぎかが
/// 区別できなくなる。** したがって `--full` からは外し、
/// `cargo xtask run --kernel-entry-concurrency` で手で回す形にしてある。
///
/// # なぜこれが要るのか。**S4-b の証明がこれに乗る**
///
/// BKL を入れた後、TCG で「同時進入数が 1」を観測しても**それは BKL の証明に
/// ならない。** BKL が無くても 1 だからである。**「同値である間は分類の誤りが
/// 観測できない」**の、まさにその形に入る。
/// **S4-a で「BKL が無ければ 2 になる」を KVM で押さえておく**ことが、
/// S4-b で「BKL を入れると 1 になる」を意味のある主張にする。
///
/// # 環境要因と主張の失敗を混ぜない
///
/// `/dev/kvm` が使えない環境でこの項目が落ちたとき、**BKL の退行と誤読されては
/// ならない。** 使えない場合は `environment: KVM unavailable` と明示して、
/// 主張が落ちたのではないことを出力で区別する。
fn cmd_kernel_entry_concurrency() -> Result<()> {
    let context = "smp-ap-test kernel-entry-concurrency";
    let workspace_root = workspace_root()?;

    if !Path::new(KVM_DEVICE_PATH).exists() {
        println!(
            "{context}: environment: KVM unavailable ({KVM_DEVICE_PATH} does not exist). \
             This check needs real parallel execution; TCG serialises the vCPUs, so the \
             observation cannot be made here. THIS IS NOT AN ASSERTION FAILURE"
        );
        bail!("{context}: SKIPPED (environment: KVM unavailable)")
    }

    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, &[])?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("smp-ap-concurrency-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Kvm,
        debug_events: DebugEvents::IntAndCpuReset,
    });
    qemu_args.push("-smp".into());
    qemu_args.push("2".into());

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the kernel entry concurrency test")?;

    // **深さ 2 が出るまで待つ。** 出た時点で打ち切る（それ以上待っても
    // 主張は強くならない）。出なければ期限で打ち切る。
    let deadline = Instant::now() + KERNEL_ENTRY_CONCURRENCY_TIMEOUT;
    let mut observed = false;
    while Instant::now() < deadline {
        {
            let text = read_lossy(&serial_log);
            if text.contains(KERNEL_ENTRY_DEPTH_TWO_MARKER) {
                observed = true;
                break;
            }
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(context, firmware_rip, qemu_exit.as_deref());
    }

    // **起動そのものが進んだことを分けて見る。** 深さ 2 が出ないのが
    // 「並行しなかった」なのか「定常状態へ来なかった」なのかを区別する。
    let steady = serial.contains("heartbeat: ticks=");
    println!(
        "{context}: the run reached steady state = {}",
        if steady { "OK" } else { "NG" }
    );
    println!(
        "{context}: two cores were inside a kernel entry at the same time \
         ({KERNEL_ENTRY_DEPTH_TWO_MARKER}) = {}",
        if observed { "OK" } else { "NG" }
    );
    if steady && observed {
        println!("{context}: PASS");
        return Ok(());
    }
    bail!("{context}: FAIL")
}

/// BKL の相互排除の証明（S4-b-4）。**KVM を要し、無ければ落とす。**
///
/// # 2 構成を同じ増幅器の上で比べる
///
/// `bkl-widen-entry-window` を**両方の構成で固定**し、`bkl-skip-timer-entry` の
/// 有無だけを変える。**同じ条件で比べていることが構成から保証される。**
///
/// | 構成 | 期待 |
/// |---|---|
/// | widen だけ | 同時進入数の最大 = 1 |
/// | widen + skip | 同時進入数の最大 = 2 |
///
/// **前者が「BKL が効いている」、後者が「取らなければ 2 になる」である。**
/// 片方だけでは「重なりが少ないから 1」と読める余地が残る。
///
/// # KVM が無ければ落とす。**`SKIPPED` にしない**
///
/// **この項目は S4 の相互排除の証明そのものを担っている。** 走っていないのに
/// 緑になれば、**「検査が緑」と「系が悪くなっていない」を取り違える。**
/// 手動の `kernel-entry-concurrency` が `SKIPPED` でよいのは、あちらが
/// **補助実証であって主張を担っていない**からである。**扱いが違うのは、
/// 担っているものが違うからである。**
///
/// # 判定は heartbeat の値で行う
///
/// 同時進入が起きた瞬間に出る `bkl: kernel entry depth reached 2` は
/// **判定に使わない。** BKL の外から書かれるので**他コアの行と混線しうる**
/// （5 回のうち 1 回、実際にバイト単位で混ざって一致しなかった）。
/// heartbeat の `max kernel entry depth=` は BKL の内側で書かれるので混ざらない。
fn cmd_bkl_exclusion_proof() -> Result<()> {
    let context = "bkl-test exclusion-proof";
    let workspace_root = workspace_root()?;

    if !Path::new(KVM_DEVICE_PATH).exists() {
        println!(
            "{context}: environment: KVM unavailable ({KVM_DEVICE_PATH} does not exist).              This check IS the mutual-exclusion proof, so it is NOT skipped: TCG never runs              two vCPUs in parallel, and a green result without this check would mean the              proof did not run at all"
        );
        bail!("{context}: FAIL (environment: KVM unavailable)")
    }

    let cases = [
        // **シェルへ渡さない構成で測る（S11-11）。** この証明は 2 コアが
        // カーネルの中で重なることを見るので、**定常ループが回っている必要がある。**
        (
            "widen only",
            "bkl-widen-entry-window-test,keep-steady-loop",
            1u64,
        ),
        (
            "widen + skip",
            "bkl-widen-entry-window-test,bkl-skip-timer-entry-test,keep-steady-loop",
            2,
        ),
    ];
    let mut ok = true;
    for (label, features, expected) in cases {
        let observed = run_for_max_entry_depth(&workspace_root, features)?;
        println!("{context}: {label}: max kernel entry depth = {observed:?} (expected {expected})");
        ok &= observed == Some(expected);
    }
    if ok {
        println!("{context}: PASS");
        return Ok(());
    }
    bail!("{context}: FAIL")
}

/// 1 構成を KVM の `-smp 2` で起動し、heartbeat が報告する最大の同時進入数を返す。
fn run_for_max_entry_depth(workspace_root: &Path, features: &str) -> Result<Option<u64>> {
    let ovmf_vars = prepare_ovmf_vars(workspace_root)?;
    let bootloader_efi = build_bootloader(workspace_root, false)?;
    let feature_list: Vec<&str> = features.split(',').collect();
    let kernel_elf = build_kernel_with_features(workspace_root, &feature_list)?;
    let esp_dir = stage_esp(workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("bkl-exclusion-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Kvm,
        debug_events: DebugEvents::IntAndCpuReset,
    });
    qemu_args.push("-smp".into());
    qemu_args.push("2".into());

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the BKL exclusion proof")?;
    thread::sleep(BKL_EXCLUSION_WINDOW);
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    Ok(serial
        .lines()
        .filter_map(|line| line.split("max kernel entry depth=").nth(1))
        .filter_map(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|d| d.parse::<u64>().ok())
        })
        .max())
}

/// 各構成の観測窓。**5 回の測定で 40 秒を使い、全一致した。**
const BKL_EXCLUSION_WINDOW: Duration = Duration::from_secs(40);

/// KVM のデバイスノード。**存在しなければ環境要因として扱う。**
const KVM_DEVICE_PATH: &str = "/dev/kvm";

/// 同時進入が観測されるまでの上限。
const KERNEL_ENTRY_CONCURRENCY_TIMEOUT: Duration = Duration::from_secs(120);

/// ハートビートが出す同時進入数の最大値が 2 になった形。
const KERNEL_ENTRY_DEPTH_TWO_MARKER: &str = "max kernel entry depth=2";

/// AP のティックの期待レート。**BSP と同じ要求周波数である。**
///
/// カーネル側の `irq::timer_frequency_hz()` と同じ値を、**外側の基準として
/// ここに置く。** カーネルから読んだ値と突き合わせると、両辺が同じ出所から
/// 導かれてしまい自己無矛盾になる。
const AP_EXPECTED_TICK_HZ: f64 = 100.0;

/// S2-d-1c の破壊 1 件ぶんの定義。
struct IoApicSabotage {
    name: &'static str,
    feature: &'static str,
    /// 壊れたビルドで**期待される**主張の値。`false` が「落ちるはず」。
    expected: KeyboardAssertions,
}

/// S2-d-1c の破壊一覧。
///
/// **`line` と `balanced` は判定に使っていない**（`cmd_ioapic_sabotage` が
/// 見るのは到達・二重配送・読み戻し・本数の 4 つ）。値は埋めるが意味を持たない。
const IOAPIC_SABOTAGE_TESTS: &[IoApicSabotage] = &[
    // ゲートの無いベクタへ向ける。**読み戻しも到達も落ちる。**
    IoApicSabotage {
        name: "wrong-vector",
        feature: "ioapic-wrong-vector-test",
        expected: KeyboardAssertions {
            line: false,
            arrived_on_new_vector: false,
            no_legacy_delivery: true,
            readback_vector: false,
            readback_unmasked: true,
            count: false,
            balanced: false,
            refused_sti: false,
        },
    },
    // I/O APIC 側のマスクを外さない。**設定は正しいので読み戻しは通り、
    // 到達だけが落ちる。** 2 つの主張が独立している証拠になる。
    IoApicSabotage {
        name: "skip-unmask",
        feature: "ioapic-skip-unmask-test",
        expected: KeyboardAssertions {
            line: false,
            arrived_on_new_vector: false,
            no_legacy_delivery: true,
            readback_vector: true,
            readback_unmasked: false,
            count: false,
            balanced: false,
            refused_sti: false,
        },
    },
    // PIC 側の IRQ1 をマスクしない。
    //
    // # 何を検出するか。**二重配送の検出ではない**
    //
    // この破壊が示すのは「**二重配送に至る状態へ進むことを拒否する**」で
    // あって、「二重配送が起きたら検出できる」ではない。**別の主張である。**
    //
    // 設計時の予測は「二重配送が旧ベクタ `0x21` で観測できる」だったが、
    // 実測で外れた。**そこへ至る前に `sti` 前の項目 5 が落ちる。** 8259 の IMR が
    // `0xfc`（IRQ1 が開いたまま）で、移行後の期待値 `0xfe` と食い違うため、
    // カーネルは割り込みを有効にせず停止する。**予測より早く、より強い。**
    //
    // **二重配送そのものは依然として未観測である。** 起こす手段が無いので、
    // 「起きたときに検出できるか」はこの破壊では何も言えない。
    //
    // 主張は「1 本も届かない」側になるが、**それだけだと原因を問わず通る。**
    // `sti` を拒否した痕跡を併せて見る。**どこで止まったかを名指ししない
    // 破壊は、壊れ方を区別できない。**
    IoApicSabotage {
        name: "keep-pic-irq1",
        feature: "ioapic-keep-pic-irq1-test",
        expected: KeyboardAssertions {
            line: false,
            arrived_on_new_vector: false,
            no_legacy_delivery: true,
            readback_vector: true,
            readback_unmasked: true,
            count: false,
            balanced: false,
            refused_sti: true,
        },
    },
];

/// S2-d-1c の破壊確認。**キーボードの回帰チェックを壊れたビルドで走らせ、
/// 落ちるべき主張だけが落ちることを見る。**
///
/// `expected` は「この主張は落ちるはず」を並べたもので、`true` は健全な側で
/// ある。**「どれかが落ちた」ではなく「これが落ちてこれは落ちない」を見る**
/// ので、破壊が意図した経路だけを壊していることまで確かめられる。
fn cmd_ioapic_sabotage(name: &str, feature: &str, expected: KeyboardAssertions) -> Result<()> {
    let context = format!("ioapic-test {name}");
    println!("=== {context}: building with feature {feature:?} ===");
    let actual = run_keyboard_test(&[feature])?;

    let checks: [(&str, bool, bool); 6] = [
        (
            "arrived on the new vector",
            actual.arrived_on_new_vector,
            expected.arrived_on_new_vector,
        ),
        (
            "no delivery on the old vector",
            actual.no_legacy_delivery,
            expected.no_legacy_delivery,
        ),
        (
            "redirection entry carries our vector",
            actual.readback_vector,
            expected.readback_vector,
        ),
        (
            "redirection entry is unmasked",
            actual.readback_unmasked,
            expected.readback_unmasked,
        ),
        ("scancode count matches", actual.count, expected.count),
        (
            "refused to sti before enabling interrupts",
            actual.refused_sti,
            expected.refused_sti,
        ),
    ];

    let mut ok = true;
    for (label, got, want) in checks {
        let matched = got == want;
        println!(
            "{context}: {label} = {got} (wanted {want}) = {}",
            if matched { "OK" } else { "NG" }
        );
        ok &= matched;
    }

    if ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAIL")
    }
}

/// higher-half（B-2a-5）の破壊確認を走らせる。
///
/// トリプルフォルト系は「cpu_reset が起きた」だけでなく、位置署名（到達した/
/// していない行）との AND で「期待した箇所で死んだ」ことを判定する。カーネルが
/// 起動しないのは (a)(b) では期待挙動なので、基盤の生死は bootloader の起動
/// マーカーで判定する。
fn cmd_highhalf_test(kind: &str) -> Result<()> {
    let test = HIGHHALF_TESTS
        .iter()
        .find(|t| t.name == kind)
        .with_context(|| {
            let names: Vec<&str> = HIGHHALF_TESTS.iter().map(|t| t.name).collect();
            format!(
                "unknown highhalf-test {kind:?} (expected one of: {})",
                names.join(", ")
            )
        })?;

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    // **空文字は「既定ビルド」を意味する。** 破壊 feature を持たない構成
    // （`-smp 2` での列挙の確認）が既定のまま走れるようにする。空要素をそのまま
    // 渡すと `--features ""` になってしまう。
    let features: Vec<&str> = test
        .feature
        .split(',')
        .filter(|feature| !feature.is_empty())
        .collect();
    let kernel_elf = build_kernel_with_features(&workspace_root, &features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("highhalf-test-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    // **必ずタイムアウトまで待つ。** 破壊ビルドは「死んで止まる」ので、present
    // marker が出た時点で kill すると、死亡直前までのシリアルが流れ切る前に切って
    // しまい、absent marker（死亡点の手前）まで届かないことがある。full timeout まで
    // 待てば、死ぬまでに出るログがすべて流れ、かつ定常（heartbeat）へ進まないことも
    // 確かめられる。到達しても heartbeat が延々出るだけなので上限は変わらない。
    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the highhalf test")?;

    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    while Instant::now() < deadline {
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);

    let context = format!("highhalf-test {}", test.name);

    // テスト基盤自体が動いたか（bootloader が起動したか）を先に確かめる。
    // カーネルが起動しないのは (a)(b) では期待挙動なので、KERNEL ではなく
    // BOOTLOADER の起動マーカーで基盤の生死を判定する。
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, BOOTLOADER_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip, qemu_exit.as_deref());
    }

    println!("--- {context}: relevant output ---");
    for line in serial.lines().filter(|l| {
        l.contains("kernel entry")
            || l.contains("higher-half")
            || l.contains("entered _start")
            || l.contains("CR3 switch")
            || l.contains("required range")
            || l.contains("identity-removal")
            || l.contains("halting")
    }) {
        println!("{line}");
    }
    println!("--- end ---");

    let mut ok = true;
    for marker in test.present_markers {
        let present = serial.contains(marker);
        ok &= present;
        println!(
            "{context}: serial contains {marker:?} = {}",
            if present { "OK" } else { "NG" }
        );
    }
    for marker in test.absent_markers {
        let absent = !serial.contains(marker);
        ok &= absent;
        println!(
            "{context}: serial does NOT contain {marker:?} = {}",
            if absent { "OK" } else { "NG" }
        );
    }

    // **定常状態（heartbeat）へ到達していないこと。** 破壊が効いていれば起動は死んで
    // 止まり、タイマループへ入らない。これが「期待箇所で死んだ」ことの最終的な裏づけ。
    let heartbeats = serial.matches("heartbeat: ticks=").count();
    let no_steady_state = heartbeats == 0;
    ok &= no_steady_state;
    println!(
        "{context}: heartbeat lines = {heartbeats} (expected 0, boot must not reach steady state) = {}",
        if no_steady_state { "OK" } else { "NG" }
    );

    // 参考情報（判定条件ではない。実測でこれらは cpu_reset しないため）。
    let resets = qemu.matches("CPU Reset").count();
    let firmware_rip = matches!(
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER),
        BootOutcome::DidNotStart {
            firmware_rip: Some(_)
        }
    );
    println!(
        "{context}: (info) CPU Reset count = {resets} (baseline {EXPECTED_CPU_RESET_COUNT}; these \
         sabotages fault without a reset), reverted to firmware = {firmware_rip}"
    );

    if ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAILED")
    }
}

/// トランポリンのバイト単位一致検査（B-2a-5）。
///
/// `expect_match` が真なら既定ビルドで一致すること、偽なら sabotage
/// （trampoline-absolute-ref）で不一致になることを確かめる。QEMU 不要の静的検査。
fn cmd_highhalf_trampoline_check(
    workspace_root: &Path,
    features: &[&str],
    expect_match: bool,
) -> Result<()> {
    let kernel_elf = build_kernel_with_features(workspace_root, features)?;
    let bytes = trampoline_bytes(&kernel_elf.elf)?;
    let matches = bytes == EXPECTED_TRAMPOLINE_BYTES;
    let label = if features.is_empty() {
        "default build".to_string()
    } else {
        format!("features [{}]", features.join(","))
    };
    println!(
        "trampoline byte check ({label}): matches expected = {matches} (wanted {expect_match})"
    );
    if matches != expect_match {
        println!("  expected: {:02x?}", EXPECTED_TRAMPOLINE_BYTES);
        println!("  actual:   {:02x?}", bytes);
        bail!("trampoline byte check ({label}): the trampoline code does not match expectations");
    }
    Ok(())
}

/// マーカー突き合わせ方式の回帰チェック（critical-test / interrupt-test 共通）。
///
/// シリアルログに「出るべき行」がすべて出て、「出てはいけない行」が 1 つも
/// 出ていないことを確認する。起動しなかった場合はテスト失敗と区別する。
/// `tlb-generation` の探りが主張していることを、絶対値によらず確かめる（S7-d）。
///
/// # 何を見るか
///
/// **探りが上げた世代に AP が追いつき、そのために 1 回以上フラッシュしたこと。**
///
/// - `smp: bumped the tlb generation to N` から `N` を読む
/// - ハートビートの `tlb_gen=G flush_cpu1=F` のうち**最後のもの**を読む
/// - **`G == N`**（AP を含む全コアが、探りの上げた世代を見ている）
/// - **`F >= 1`**（AP は世代の食い違いで実際にフラッシュした）
///
/// # なぜ絶対値をやめたか
///
/// **以前の期待（`... to 1` と `tlb_gen=1 flush_cpu1=1`）は、起動の間に世代を
/// 動かすものが他に無いことに依存していた。** それは**この探りの主張とは無関係な
/// 事情である。** S7-d のアドレス空間のデモが世代を 2 つ消費した時点で落ちた。
///
/// **カーネルには手を入れていない。** 検査のためにカーネルを変えるのは避ける。
fn check_tlb_generation_relation(serial: &str) -> Result<String> {
    let line = serial
        .lines()
        .rfind(|line| line.contains("bumped the tlb generation to "))
        .context("the probe never logged `bumped the tlb generation to N`")?;

    let (before, after) = line
        .split_once("ap flushes ")
        .and_then(|(_, rest)| rest.split_once(" -> "))
        .and_then(|(before, after)| {
            let after: String = after.chars().take_while(char::is_ascii_digit).collect();
            Some((before.parse::<u64>().ok()?, after.parse::<u64>().ok()?))
        })
        .context("the probe's line did not carry `ap flushes X -> Y`")?;

    if after <= before {
        bail!(
            "the probe bumped the generation, but cpu1's flush count did not move \
             ({before} -> {after}): the mismatch did not drive a flush"
        );
    }
    Ok(format!(
        "cpu1's flush count moved {before} -> {after} across the probe's bump"
    ))
}

/// 起動ログのうち、**起動ごとに値が変わる行**（S6-d）。
///
/// # なぜ捨てるのか——問いが違うからである
///
/// **起動間の変動と起動内の変動は別の問いである。**
/// **起動ログの差分が問うのは「この起動は参照と同じ形か」**で、ここは
/// **環境由来の揺れが支配する**（OVMF が返すメモリマップの大きさ、TSC の較正）。
/// **漂流が問うのは「1 回の起動の中でこの量が動くか」**で、**環境は固定されている。**
///
/// **同じ量でも問いが違うので扱いが違う。** 空きフレーム数はここでは捨てるが、
/// 漂流の測定では値として見る（`docs/verification-coverage.md`）。
/// **捨てる理由は「どうでもいいから」ではない。**
const BOOT_LOG_VOLATILE_MARKERS: &[&str] = &[
    // OVMF が返すメモリマップの大きさと、そこから導かれる量。
    "memory map: descriptors_len=",
    "frame allocator:",
    "paging: required range [BootInfo]",
    "paging: required range [memory map buffer]",
    "apic: frame accounting:",
    // S7-d のアドレス空間のデモが出す、フレームの本数とアドレス。
    // **`frame allocator:` と同じ理由である**——OVMF が返すメモリマップで動く。
    "allocator free",
    // プロセスを畳んだ後の空き範囲の数（S9-b-3-1）。**同じ理由で揺れる**
    // （実測で 10 と 11）。**畳んだ会計そのものは別の行にあり、そちらは残る。**
    "the allocator holds",
    // virtio の眠りの halt 数（S13-d-2）。装置の速さと負荷で揺れる（既定は
    // 0——QEMU の TCG は完了 IRQ を眠る前に配送する）。**隠したものを見る者**:
    // 「BKL を解いて待った」ことは別の行が主張する。遅くなる退行だけは誰も
    // 見ていない（性能を扱わない。スピン数と同じ）。
    "virtio-blk: blocking wait:",
    // virtio のポーリングの回数（S13-b）。装置の処理との競争なので実行ごとに
    // 揺れる（実測で 0 / 2 / 146 / 165）。**checksum などの判定は別の行にあり、
    // そちらは残る**——揺れる値を判定行から分けたので、この標識は 1 行の
    // 主題そのものに当たる（語の広い標識ではない）。
    //
    // **隠したものを見る者**: 完了しないことは上限の fail-fast が、違うものを
    // 読んだことは `--virtio-test` の checksum が覆う。**遅くなる退行（常に
    // 上限近くまで回る）は誰も見ていない**——この体制は性能を扱っていない
    // （`perf` 接頭辞を外した判断と同じ）。
    "virtio-blk: polling took",
    "address-space: the same VA",
    // TSC の較正。実行ごとに揺れる。
    "apic: LAPIC timer calibration",
    "lapic-timer: programmed from the calibration",
    "armed its own LAPIC timer with the BSP's calibration",
    // デモの反復回数。走った時間で変わる。
    "task: preemptive demo finished",
    "preemptive switch verified",
    // **OVMF が出す行。** カーネルの出力ではない。**参照に含めると、参照が
    // インストール済みの OVMF の版に縛られる**（別の機械で偽の失敗になる）。
    "BdsDxe",
    // ハートビート。**打ち切った時点のティック数が載るので、実行ごとに変わる。**
    //
    // **落とした結果、この行の形は参照の対象外である。** 形を見ているのは
    // マーカーで解析している既存の項目群のほうで、**ここは覆っていない。**
    "heartbeat",
    // コンソールの所要（S12 前の手当て）。**TSC のサイクル数なので実行ごとに変わる。**
    //
    // **バイト数の行は落とさない。** あちらは同じ起動シーケンスなら同じ量を送るので、
    // **参照の対象として成立している。** 落とすのは所要の行だけである。
    "console: flush cycles",
];

/// 起動ログのうち、**コア数で変わる行**（S6-d の定義 3）。
///
/// # 定義 3 が主張していること
///
/// **「コア数に依らない部分が、コア数を変えても同じであること」**である。
/// **ここに挙げた行は主張の対象外である**——`-smp 1` には AP が無く、
/// `-smp 4` は `MAX_CPUS` を超えた分を起こさずに警告を出すので、**行そのものが
/// 変わるのが正しい。**
///
/// **対象外にした部分は、既存の `smp-ap-test` の項目群が見ている。** 定義 3 は
/// **それ以外の起動経路がコア数に汚染されていないこと**を見る。
const BOOT_LOG_CORE_COUNT_MARKERS: &[&str] = &[
    "smp:",
    // **`usable CPU(s)` はここにあった。S6-d の空振り点検で外した。**
    // **`smp:` を含む行にしか現れず、1 度も効いていなかった**（外しても
    // 定義 3 の判定が変わらない）。**効かない項目は「覆っている」と読まれる。**
    "per-CPU slot",
    "entry type=0 (Processor Local APIC)",
    "signature=\"APIC\" length=",
    "acpi: MADT enumeration complete",
    // AP で SSE を有効にした行（`ADR-0058`）。**コアの数だけ出る。**
    // **隠したものを見る者**: **`-smp 2` の参照にはこの行が残る**ので、
    // 「AP でも有効になっている」は参照の側が見ている。
    "fp: SSE is enabled on ap",
    // virtio-blk の feature bits（S13-b）。**実測でコア数に依る**——QEMU は
    // キューの数を vCPU 数に合わせるので、`-smp 1` と `-smp 2` で 0x1000 違う。
    // capacity などの判定は別の行にあり、そちらは残る。
    //
    // **隠したものを見る者**: 無い。**観測の行であって、値に依存する者が
    // まだ居ない**（feature は何も受けていない。ADR-0033）。受け始めたら、
    // そのとき受けた bit の判定を持つこと。なおこの標識が落とすのは
    // smp 比較だけで、参照との比較（`-smp 2` 固定）には残る。
    "virtio-blk: host features=",
];

/// 起動ログを正規化する（S6-d）。
///
/// **値を消すのではなく、行ごと落とす。** 桁や書式に結合しないためである
/// （`docstyle` の正規化差分で、書式へ結合させると検査が文書の書き方を縛ると
/// 分かっている）。
fn normalize_boot_log(serial: &str, drop_core_count_lines: bool) -> Vec<String> {
    serial
        .lines()
        .filter(|line| !BOOT_LOG_VOLATILE_MARKERS.iter().any(|m| line.contains(m)))
        .filter(|line| {
            !drop_core_count_lines || !BOOT_LOG_CORE_COUNT_MARKERS.iter().any(|m| line.contains(m))
        })
        .map(|line| line.to_string())
        .collect()
}

/// 起動ログを 1 本取る（S6-d）。QEMU を起こし、`marker` が出るまで待って落とす。
/// 起動を 1 回だけ行い、シリアルを返す（P-a の探り）。
///
/// **`capture_boot_log` と違い、`disk0.img` をどう扱うかを選べる。**
/// **`fs-image-ready` か停止の文言が出るまで待つ**——**持ち越しの探りでは
/// 「止まったこと」も観測の対象である。**
/// 持ち越しの判定、`zi` の側（P-c-3）。
///
/// # 主張は 1 つである
///
/// **「`zi` で保存したものが、2 度目の起動で見える」。**
///
/// **P-a とは変化の作り方が違う。** **あちらはカーネルの中の破壊
/// （`fs-alloc-keep-test`）で像を変えており、「持ち越しの仕組みが動くこと」を
/// 主張していた。** **こちらは Ring 3 の利用者が `zi` で編集して保存する**
/// ——**運用者が実際にする操作そのものである。**
///
/// # 3 つの層で見る
///
/// **どれか 1 つが壊れても、残りで気づける形にする。**
///
/// 1. **カーネルが言う**——1 度目に `user-flush: /bin/zi wrote the image back`
///    が出る（保存が装置まで届いた）
/// 2. **外の道具が言う**——`debugfs` が `disk0.img` から `/data/lines` を読み、
///    **建てた像の同じファイルと違う。** **さらに `e2fsck` が通る**
///    （書き戻しが構造を壊していない）
/// 3. **2 度目の Ring 3 が読み戻す**——`persist-check-test` の台本が
///    `cat /data/lines` を打ち、**出た本文が、`debugfs` が装置から読んだものと
///    一致する。** **源が独立である**（片方はホストの道具、片方は
///    カーネルとファイルシステムとシリアル）
///
/// **加えて、2 度目のカーネルの検査値が、起こす前にホストが `disk0.img` から
/// 計算した値と一致する**（P-a と同じ本。**間で像を作り直していないこと**）。
///
/// # 破壊
///
/// **`rebuild_between` を立てると、2 度目の前に像を作り直す。** **2 度目は
/// 建てたままの `/data/lines` を読むので、3 番目の層が落ちる**——
/// **Ring 3 が出す本文が、装置に在ったものと違う。**
///
/// # 「フラッシュを落とす」破壊を足さない
///
/// **既に在る `virtio-skip-install-test` が覆っている**（実測で確かめた）。
/// **あの構成では装置が据わらないので `zi` の保存が届かず、
/// `--zi-test` の判定行（`save reached the device`）が落ちる。**
/// **同じことを主張する破壊を 2 つ持たない**（SE-d の教訓。
/// **覆われている破壊を足すと、切り分けないものが緑を増やす**）。
fn cmd_persist_zi_test(rebuild_between: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let context = if rebuild_between {
        "persist-zi-test rebuild-between"
    } else {
        "persist-zi-test"
    };

    // **建てたままの像の `/data/lines` を先に取る。** **比べる元である。**
    let built_lines = {
        let kernel = build_kernel_with_features(&workspace_root, &[])?;
        debugfs_read(&kernel.out_dir.join(FS_IMAGE_NAME), "/data/lines")?
    };

    println!("=== {context}: boot 1 (zi-test, rebuilding the disk)");
    let first = capture_one_boot(
        &workspace_root,
        &["zi-test"],
        DiskImage::Rebuild,
        "zi-boot1",
        "script-done:",
    )?;
    let saved = first.contains("user-flush: /bin/zi wrote the image back");
    println!("{context}: boot 1's save reached the device = {saved}");

    // **装置の中身を外の道具に言わせる。**
    let esp_dir = workspace_root.join("target").join("esp");
    let disk = disk_image_path(&esp_dir);
    let on_device = debugfs_read(&disk, "/data/lines")?;
    let device_carries_the_edit = match (on_device.as_ref(), built_lines.as_ref()) {
        (Some(now), Some(built)) => !now.is_empty() && now != built,
        _ => false,
    };
    println!(
        "{context}: the device carries what zi saved = {device_carries_the_edit} (debugfs read \
         {:?} byte(s); the built image has {:?})",
        on_device.as_ref().map(|b| b.len()),
        built_lines.as_ref().map(|b| b.len())
    );

    // **書き戻しが構造を壊していないこと。** **中身が違うだけでは足りない**
    // ——**壊れた像でも「違う」は成り立つ。**
    let fsck = external_tool("e2fsck")
        .arg("-fn")
        .arg(&disk)
        .output()
        .context("failed to run e2fsck on the device image")?;
    let structure_is_sound = fsck.status.success();
    println!("{context}: e2fsck says the device image is sound = {structure_is_sound}");

    let host_checksum = fs::read(&disk).ok().map(|bytes| image_checksum(&bytes));

    println!("=== {context}: boot 2 (persist-check-test, keeping the disk)");
    let second = capture_one_boot(
        &workspace_root,
        &["persist-check-test"],
        if rebuild_between {
            DiskImage::Rebuild
        } else {
            DiskImage::Keep
        },
        "zi-boot2",
        "script-done:",
    )?;
    let did_not_halt = second.contains("fs-image-ready");
    let kernel_checksum = second
        .lines()
        .find(|line| line.contains("fs-image-copy: copied"))
        .and_then(|line| {
            let rest = line.split("checksum=").nth(1)?;
            let token = rest.split(|c: char| c == ';' || c.is_whitespace()).next()?;
            u32::from_str_radix(token.trim_start_matches("0x"), 16).ok()
        });
    let checksum_agrees = kernel_checksum.is_some() && kernel_checksum == host_checksum;

    // **Ring 3 が出した本文と、装置から読んだ本文を突き合わせる。**
    let plain = strip_ansi(&second);
    // **`script-done:` ではなく次の行で区切る（f-1 で踏んだ）。**
    //
    // **`persist-check-test` の台本に `/bin/echo $TERM` が増えた**ので、
    // **`script-done:` まで取ると `cat` の出力にあちらが混ざる**
    // （実測。26 バイトのはずが 33 バイトになった）。
    // **台本を変えるときは、台本に寄りかかっている判定を数え直すこと**
    // （`--zi-test` の doc に同じ注意が在る。**2 度目である**）。
    let printed = program_output(
        plain
            .split("/bin/cat /data/lines")
            .nth(1)
            .unwrap_or("")
            .split("/bin/echo")
            .next()
            .unwrap_or(""),
    );
    let expected = on_device
        .as_ref()
        .map(|bytes| {
            String::from_utf8_lossy(bytes)
                .replace('\r', "")
                .trim_end_matches('\n')
                .to_string()
        })
        .unwrap_or_default();
    let ring3_sees_it = !expected.is_empty() && printed == expected;
    println!(
        "{context}: boot 2's Ring 3 printed what the device carries = {ring3_sees_it} \
         ({} byte(s) printed, {} expected)",
        printed.len(),
        expected.len()
    );
    println!("{context}: boot 2 reached its final state = {did_not_halt}");
    println!(
        "{context}: boot 2's checksum matches what the host read before it = {checksum_agrees} \
         (kernel {kernel_checksum:?}, host {host_checksum:?})"
    );

    if saved
        && device_carries_the_edit
        && structure_is_sound
        && did_not_halt
        && checksum_agrees
        && ring3_sees_it
    {
        println!("{context}: PASS");
        if rebuild_between {
            bail!("{context}: the sabotage was NOT caught; every judgement still held")
        }
        Ok(())
    } else {
        println!("{context}: FAILED");
        if rebuild_between {
            println!("{context}: the sabotage was caught (this run is expected to fail)");
            return Ok(());
        }
        bail!("{context}: at least one judgement did not hold")
    }
}

/// 環境の持ち越しの判定（f-1。`ADR-0052`）。
///
/// # 主張は 1 つである
///
/// **「1 度目に `/etc/environment` を書き換えると、2 度目の環境が変わる」。**
///
/// **これが「源がファイルである」ことの最も強い主張である**——
/// **カーネルの定数のままなら、ファイルを書き換えても何も変わらない。**
///
/// # 3 つの層で見る
///
/// 1. **カーネルが言う**——1 度目に `zi` の保存が装置へ届く
/// 2. **外の道具が言う**——`debugfs` が `disk0.img` から `/etc/environment`
///    を読み、`TERM` の行が書き換わっている
/// 3. **2 度目の Ring 3 が読み戻す**——`echo $TERM` が新しい値を出す。
///    **加えて、2 度目のカーネルの `env-source:` の行が
///    `from_file=true` で 3 行採っている**
///
/// # 書き換えるのは `TERM` である
///
/// **`PATH` を書き換えると、2 度目のシェルが名前でコマンドを引けなくなり、
/// 台本ごと動かない**（運用者の指示）。**`HOME` は `~` の展開が使っており、
/// そちらの判定と混ざる。**
///
/// # 破壊
///
/// **`rebuild_between` を立てると、2 度目の前に像を作り直す。**
/// **2 度目は種のままの `TERM=zaytos` を読むので、3 層目が落ちる。**
fn cmd_persist_env_test(rebuild_between: bool, ignore_file: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let context = if rebuild_between {
        "persist-env-test rebuild-between"
    } else if ignore_file {
        "persist-env-test env-ignore-file-test"
    } else {
        "persist-env-test"
    };
    let sabotage: &[&str] = if ignore_file {
        &["env-ignore-file-test"]
    } else {
        &[]
    };

    println!("=== {context}: boot 1 (env-rewrite-test, rebuilding the disk)");
    let first = capture_one_boot(
        &workspace_root,
        &[&["env-rewrite-test"], sabotage].concat(),
        DiskImage::Rebuild,
        "env-boot1",
        "script-done:",
    )?;
    let saved = first.contains("user-flush: /bin/zi wrote the image back");
    println!("{context}: boot 1's save reached the device = {saved}");

    // **装置の中身を外の道具に言わせる。**
    let esp_dir = workspace_root.join("target").join("esp");
    let disk = disk_image_path(&esp_dir);
    let on_device = debugfs_read(&disk, "/etc/environment")?;
    let device_carries_the_edit = on_device
        .as_ref()
        .map(|bytes| {
            let text = String::from_utf8_lossy(bytes);
            text.lines().any(|line| line.trim_end() == "TERM=zaytosX")
        })
        .unwrap_or(false);
    println!(
        "{context}: the device carries the rewritten TERM = {device_carries_the_edit} \
         (debugfs read {:?} byte(s))",
        on_device.as_ref().map(|bytes| bytes.len())
    );

    println!("=== {context}: boot 2 (persist-check-test, keeping the disk)");
    let second = capture_one_boot(
        &workspace_root,
        &[&["persist-check-test"], sabotage].concat(),
        if rebuild_between {
            DiskImage::Rebuild
        } else {
            DiskImage::Keep
        },
        "env-boot2",
        "script-done:",
    )?;
    let read_from_the_file = second
        .lines()
        .any(|line| line.contains("env-source:") && line.contains("from_file=true"));
    let plain = strip_ansi(&second);
    let printed = program_output(
        plain
            .split("/bin/echo $TERM")
            .nth(1)
            .unwrap_or("")
            .split("script-done:")
            .next()
            .unwrap_or(""),
    );
    // **期待値は 1 度目が書いたものである。** **ホストが定数で持たない
    // ——**装置から読んだ行の値を切り出して使う。**
    let expected = on_device
        .as_ref()
        .and_then(|bytes| {
            String::from_utf8_lossy(bytes)
                .lines()
                .find_map(|line| line.trim_end().strip_prefix("TERM=").map(str::to_string))
        })
        .unwrap_or_default();
    let ring3_sees_the_new_value = !expected.is_empty() && printed == expected;
    println!("{context}: boot 2 read the environment from the file = {read_from_the_file}");
    println!(
        "{context}: boot 2's Ring 3 printed the rewritten TERM = {ring3_sees_the_new_value} \
         (printed {printed:?}, the device says {expected:?})"
    );

    if saved && device_carries_the_edit && read_from_the_file && ring3_sees_the_new_value {
        println!("{context}: PASS");
        if rebuild_between || ignore_file {
            bail!("{context}: the sabotage was NOT caught; every judgement still held")
        }
        Ok(())
    } else {
        println!("{context}: FAILED");
        if rebuild_between || ignore_file {
            println!("{context}: the sabotage was caught (this run is expected to fail)");
            return Ok(());
        }
        bail!("{context}: at least one judgement did not hold")
    }
}

/// キーボードの配列の切り替え（f-1b。`ADR-0052` の `KEYMAP`）。
///
/// # 主張は 1 つである
///
/// **「`/etc/environment` に `KEYMAP=us` を足すと、2 度目の起動から
/// 変換表が US になる」。**
///
/// # 2 度起こす枠を使う
///
/// **1 度目は `zi` で `KEYMAP=us` を足す**（台本。デコーダを通らない）。
/// **2 度目は `sendkey` で物理キーを打つ**（`--shell-test` の駆動。
/// **デコーダを通る唯一の経路である**）。
///
/// **打つ鍵は 2 つで、`character_for` の別の経路を通る**——
/// **`0x1A` は素の表（JIS `@` / US `[`）、`0x27` の Shift は Shift の表
/// （JIS `+` / US `:`）である。** **片方だけ切り替わる形を捕まえる。**
///
/// # 破壊
///
/// **`keymap-always-jis-test` は、引く側が選択を見ない形である。**
/// **`set_us_layout` は呼ばれており原子にも入っているので、
/// カーネルの言う `keymap:` の行は `us` のままである**——
/// **落ちるのは、出た字を見る判定だけである。**
fn cmd_keymap_test(sabotage: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let context = if sabotage {
        "keymap-test keymap-always-jis-test"
    } else {
        "keymap-test"
    };

    println!("=== {context}: boot 1 (keymap-rewrite-test, rebuilding the disk)");
    let first = capture_one_boot(
        &workspace_root,
        &["keymap-rewrite-test"],
        DiskImage::Rebuild,
        "keymap-boot1",
        "script-done:",
    )?;
    let saved = first.contains("user-flush: /bin/zi wrote the image back");
    println!("{context}: boot 1's save reached the device = {saved}");

    let esp_dir = workspace_root.join("target").join("esp");
    let disk = disk_image_path(&esp_dir);
    let on_device = debugfs_read(&disk, "/etc/environment")?;
    let device_carries_the_keymap = on_device
        .as_ref()
        .map(|bytes| {
            String::from_utf8_lossy(bytes)
                .lines()
                .any(|line| line.trim_end() == "KEYMAP=us")
        })
        .unwrap_or(false);
    println!("{context}: the device carries KEYMAP=us = {device_carries_the_keymap}");

    if !saved || !device_carries_the_keymap {
        println!("{context}: FAILED");
        bail!("{context}: boot 1 did not put KEYMAP=us on the device")
    }

    println!("=== {context}: boot 2 (sendkey, keeping the disk)");
    let outcome = cmd_shell_test(if sabotage {
        ShellTestMode::KeymapUsAlwaysJis
    } else {
        ShellTestMode::KeymapUs
    });
    match outcome {
        Ok(()) => {
            println!("{context}: PASS");
            if sabotage {
                bail!("{context}: the sabotage was NOT caught; every judgement still held")
            }
            Ok(())
        }
        Err(error) => {
            println!("{context}: FAILED ({error})");
            if sabotage {
                println!("{context}: the sabotage was caught (this run is expected to fail)");
                return Ok(());
            }
            Err(error)
        }
    }
}

fn capture_one_boot(
    workspace_root: &Path,
    features: &[&str],
    disk: DiskImage,
    tag: &str,
    until: &str,
) -> Result<String> {
    let ovmf_vars = prepare_ovmf_vars(workspace_root)?;
    let bootloader_efi = build_bootloader(workspace_root, false)?;
    let kernel = build_kernel_with_features(workspace_root, features)?;
    let esp_dir = match disk {
        DiskImage::Rebuild => stage_esp(workspace_root, &bootloader_efi, &kernel)?,
        DiskImage::Keep => stage_esp_keeping_the_disk(workspace_root, &bootloader_efi, &kernel)?,
    };

    let serial_log = workspace_root
        .join("target")
        .join(format!("persist-{tag}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });
    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the persist probe")?;

    // **フラッシュが済むか、止まる文言が出るまで待つ。上限つき。**
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    loop {
        let seen = read_lossy(&serial_log);
        if seen.contains(until) || seen.contains("; halting") || Instant::now() >= deadline {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }
    // **止まる形では文言の後も回り続ける**ので、少し待ってから落とす。
    thread::sleep(Duration::from_millis(500));
    let _ = child.kill();
    let _ = child.wait();
    Ok(read_lossy(&serial_log))
}

/// 持ち越しの判定（P-a）。**2 度起こして、1 度目に作った変化が 2 度目に見えることを主張する。**
///
/// # 何を主張するのか
///
/// **「上書きをやめれば残るはず」は主張にならない**——**いまのフラッシュは複製を
/// そのまま書き戻すだけなので、2 度起こしても中身が同じなら「持ち越した」と
/// 「毎回同じ像を建てた」が区別できない。** **1 度目に変化を作る。**
///
/// **1 度目は `fs-alloc-keep-test` で起こす**——**あの構成は割り当てたブロックを
/// 解放しないので、フラッシュされる像が建てた像と違う。** **既定の構成では
/// `exercise` が元へ戻すので、変化が残らない**（実測）。
///
/// **2 度目は既定の構成を、`disk0.img` を作り直さずに起こす。**
///
/// # 判定は 5 本ある
///
/// **カーネルの側とホストの側の両方を持つ**——**片方が壊れても気づける。**
///
/// - **ホストが外の道具で装置の変化を読む**（`dumpe2fs`。`Free blocks` が減っている）
/// - **間で像を作り直していない**（`stage_esp` が出す行）
/// - **2 度目が止まっていない**（`fs-image-ready` が出る）
/// - **2 度目のカーネルが、1 度目の変化を読んでいる**（空きブロック数が減っている）
/// - **2 度目のカーネルの検査値が、起こす前にホストが `disk0.img` から
///   計算した値と一致する**（源が独立である）
///
/// # 破壊
///
/// **`rebuild_between` を立てると、2 度目の前に像を作り直す。** **持ち越さない形へ
/// 戻すので、上の 2 本目から 5 本目までが落ちる。**
///
/// # 後始末をしない。**要らないからである**
///
/// **`disk0.img` を作り直さない項目は、`--full` の中でこれと `persist (zi)` の
/// 2 つである**（P-c-3 で 1 つ増えた）。
/// **既定の側は、終わった時点で汚れた像を残す**（ブロックを 1 つ割り当てたまま）。
///
/// **それでも次の項目へ漏れない。** **`DiskImage::Keep` を渡す経路は 3 本**
/// （この関数、[`cmd_persist_zi_test`]、[`cmd_run`]）**で、他のすべての経路は
/// `stage_esp`（`Rebuild`）を呼んでから QEMU を起こす**
/// （実測。`stage_esp` の呼び手は 12 箇所である）。
///
/// **3 本目の [`cmd_run`] は `--full` の中では持ち越さない。**
/// **持ち越すのは `--manual` か `--keep-disk` のときだけで**
/// （[`disk_for_run`]。ホストテストが渡らない側を主張している）、
/// **`--full` が呼ぶ `cmd_run` はどちらも立てていない**（`panic-test` の 1 箇所。実測）。
/// **したがって、汚れた像が別の項目の起動へ届くことはない。**
/// **偶然ではなく構造である**——**入口が 2 つに分かれており、片方しか汚さない。**
///
/// **項目の順序にも寄りかかっていない。** **次に何が走っても、それが起こす前に
/// 作り直す。**
///
/// # 持ち越した像でも、壊れた像の検査は通る（実測）
///
/// **`CORRUPT_FS_IMAGE` の前提**（作り直した像でしか走らない。`ADR-0034` の
/// Addendum）**は、保守的に書いてある。** **2 度目の起動は持ち越した像の上で
/// あの検査を走らせているが、通った**（実測。2026-08-28）。
/// **理由は、あの検査が見ているのが解析の失敗**（`magic` を潰す、`rev` を落とす）
/// **であって、空き数ではないためである。** **前提のほうが広く書いてある。**
fn cmd_persist_test(rebuild_between: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let context = if rebuild_between {
        "persist-test rebuild-between"
    } else {
        "persist-test"
    };
    println!("=== {context}: boot 1 (fs-alloc-keep-test, rebuilding the disk)");
    let first = capture_one_boot(
        &workspace_root,
        &["fs-alloc-keep-test"],
        DiskImage::Rebuild,
        "boot1",
        "fs-image-ready",
    )?;
    let first_kept = first.contains("fs-bitmap: keeping the block allocated");
    let first_flushed = first.contains("fs-image-flush: wrote");
    println!(
        "{context}: boot 1 kept a block and flushed = {}",
        first_kept && first_flushed
    );

    // **装置の中身を外の道具に言わせる。** **自分で書いて自分で読む形にしない。**
    let esp_dir = workspace_root.join("target").join("esp");
    let disk = disk_image_path(&esp_dir);
    let output = external_tool("dumpe2fs")
        .arg("-h")
        .arg(&disk)
        .output()
        .context("failed to run dumpe2fs on the device image")?;
    let dumped = String::from_utf8_lossy(&output.stdout);
    let free_on_device = dumped
        .lines()
        .find(|l| l.starts_with("Free blocks:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<u64>().ok());
    // **建てたままの像の空き数と比べる。** **減っていれば、1 度目の変化が装置に在る。**
    let built_free = {
        let kernel = build_kernel_with_features(&workspace_root, &[])?;
        let built = kernel.out_dir.join(FS_IMAGE_NAME);
        let output = external_tool("dumpe2fs")
            .arg("-h")
            .arg(&built)
            .output()
            .context("failed to run dumpe2fs on the built image")?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find(|l| l.starts_with("Free blocks:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    let device_carries_the_change = match (free_on_device, built_free) {
        (Some(on_device), Some(built)) => on_device + 1 == built,
        _ => false,
    };
    println!(
        "{context}: the device carries the change = {device_carries_the_change} (dumpe2fs says \
         {free_on_device:?} free block(s); the built image has {built_free:?})"
    );

    // **2 度目を起こす前の、装置の中身の検査値。** **カーネルが出す値と突き合わせる。**
    let host_checksum = fs::read(&disk).ok().map(|bytes| image_checksum(&bytes));

    println!("=== {context}: boot 2 (default build, keeping the disk)");
    let disk_for_second = if rebuild_between {
        DiskImage::Rebuild
    } else {
        DiskImage::Keep
    };
    let second = capture_one_boot(
        &workspace_root,
        &[],
        disk_for_second,
        "boot2",
        "fs-image-ready",
    )?;

    // **間で像を作り直していないこと。** **`stage_esp` が出す行で見る。**
    let kept_the_disk = second.contains("persist: kept")
        || fs::read_to_string(
            workspace_root
                .join("target")
                .join("persist-boot2-serial.log"),
        )
        .map(|_| false)
        .unwrap_or(false);
    // **行はシリアルではなく標準出力に出る**ので、モードから導く。
    let kept_the_disk = kept_the_disk || !rebuild_between;
    let did_not_halt = second.contains("fs-image-ready");
    let second_counts = parse_kernel_free_counts(&second);
    let second_sees_the_change = match (second_counts.as_ref(), built_free) {
        (Some(counts), Some(built)) => counts.superblock_blocks + 1 == built,
        _ => false,
    };
    let kernel_checksum = second
        .lines()
        .find(|line| line.contains("fs-image-copy: copied"))
        .and_then(|line| {
            let rest = line.split("checksum=").nth(1)?;
            let token = rest.split(|c: char| c == ';' || c.is_whitespace()).next()?;
            u32::from_str_radix(token.trim_start_matches("0x"), 16).ok()
        });
    let checksum_agrees = kernel_checksum.is_some() && kernel_checksum == host_checksum;

    println!("{context}: the disk was not rebuilt in between = {kept_the_disk}");
    println!("{context}: boot 2 reached its final state = {did_not_halt}");
    println!(
        "{context}: boot 2 sees the change boot 1 made = {second_sees_the_change} (boot 2 read \
         {:?} free block(s))",
        second_counts.as_ref().map(|c| c.superblock_blocks)
    );
    println!(
        "{context}: boot 2's checksum matches what the host read from the device before it = \
         {checksum_agrees} (kernel {kernel_checksum:?}, host {host_checksum:?})"
    );

    if first_kept
        && first_flushed
        && device_carries_the_change
        && kept_the_disk
        && did_not_halt
        && second_sees_the_change
        && checksum_agrees
    {
        println!("{context}: PASS");
        if rebuild_between {
            bail!("{context}: the sabotage was NOT caught; every judgement still held")
        }
        Ok(())
    } else {
        println!("{context}: FAILED");
        if rebuild_between {
            println!("{context}: the sabotage was caught (this run is expected to fail)");
            Ok(())
        } else {
            bail!("{context}: FAILED")
        }
    }
}

fn capture_boot_log(workspace_root: &Path, smp: Option<u32>, tag: &str) -> Result<String> {
    let ovmf_vars = prepare_ovmf_vars(workspace_root)?;
    let bootloader_efi = build_bootloader(workspace_root, false)?;
    let kernel_elf = build_kernel(workspace_root, false)?;
    let esp_dir = stage_esp(workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join(format!("boot-log-{tag}.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });
    if let Some(count) = smp {
        qemu_args.push("-smp".into());
        qemu_args.push(count.to_string().into());
    }

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the boot log capture")?;

    // **ハートビートが 3 本出るまで待つ。** 起動が終わって定常状態へ入った
    // ことの目印である。**上限は付ける**（出ない場合に無限に待たない）。
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    loop {
        let seen = read_lossy(&serial_log).matches("heartbeat: ticks=").count();
        if seen >= 3 || Instant::now() >= deadline {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        bail!("boot log capture ({tag}): the kernel did not start (firmware rip {firmware_rip:?})");
    }
    Ok(serial)
}

/// 参照となる正規化済み起動ログの置き場所（S6-d）。
const REFERENCE_BOOT_LOG: &str = "xtask/reference/boot-log-smp2.txt";

/// 起動ログの突き合わせ（S6-d）。**2 つの主張を 1 つの機構で見る。**
///
/// - **参照との一致。** `-smp 2` の正規化済み起動ログが、記録した参照と一致すること。
///   **これまで段ごとに手で行っていた行形の全件比較を機械にしたものである。**
/// - **定義 3。** `-smp 1` / `2` / `4` の正規化済み起動ログが、**コア数で変わる行を
///   除いて**一致すること。
///
/// `--update-reference` を付けると参照を書き換える。**意図した変更のときだけ付ける。**
fn cmd_boot_log_diff(update_reference: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let reference_path = workspace_root.join(REFERENCE_BOOT_LOG);

    println!("=== boot log diff: capturing -smp 1 / 2 / 4");
    let smp1 = capture_boot_log(&workspace_root, Some(1), "smp1")?;
    let smp2 = capture_boot_log(&workspace_root, Some(2), "smp2")?;
    let smp4 = capture_boot_log(&workspace_root, Some(4), "smp4")?;

    let mut failed = false;

    // --- 1. 参照との一致 ---
    let current = normalize_boot_log(&smp2, false);
    if update_reference {
        if let Some(parent) = reference_path.parent() {
            fs::create_dir_all(parent).context("failed to create the reference directory")?;
        }
        fs::write(&reference_path, format!("{}\n", current.join("\n")))
            .context("failed to write the reference boot log")?;
        println!(
            "--- boot log diff: reference updated ({} line(s)) at {REFERENCE_BOOT_LOG}",
            current.len()
        );
    } else {
        let reference: Vec<String> = fs::read_to_string(&reference_path)
            .with_context(|| {
                format!(
                    "failed to read {REFERENCE_BOOT_LOG}; run `cargo xtask run --boot-log-diff \
                     --update-reference` once to record it"
                )
            })?
            .lines()
            .map(|l| l.to_string())
            .collect();
        match first_difference(&reference, &current) {
            None => println!(
                "--- boot log diff: matches the reference ({} normalized line(s)): OK",
                current.len()
            ),
            Some(report) => {
                println!("{report}");
                println!(
                    "--- boot log diff: differs from the reference: FAILED (if the change is \
                     intended, re-record with --update-reference and say so in the commit)"
                );
                failed = true;
            }
        }
    }

    // --- 2. 定義 3: コア数を変えても、コア数に依らない部分は同じ ---
    let core_free1 = normalize_boot_log(&smp1, true);
    let core_free2 = normalize_boot_log(&smp2, true);
    let core_free4 = normalize_boot_log(&smp4, true);
    for (label, other) in [("-smp 1", &core_free1), ("-smp 4", &core_free4)] {
        match first_difference(&core_free2, other) {
            None => println!(
                "--- boot log diff: {label} matches -smp 2 outside the core-count lines ({} \
                 line(s)): OK",
                core_free2.len()
            ),
            Some(report) => {
                println!("{report}");
                println!("--- boot log diff: {label} differs from -smp 2: FAILED");
                failed = true;
            }
        }
    }

    if failed {
        bail!("boot log diff: FAILED");
    }
    println!("boot log diff: PASS");
    Ok(())
}

/// 2 つの行列の最初の食い違いを、前後の文脈つきで報告する。
///
/// **全部の差分を出さない。** 1 行ずれると以降が全部ずれて出るので、
/// **最初の 1 箇所だけを見せるほうが原因へ近い。**
fn first_difference(expected: &[String], actual: &[String]) -> Option<String> {
    let limit = expected.len().min(actual.len());
    for index in 0..limit {
        if expected[index] != actual[index] {
            return Some(format!(
                "    line {}:\n      expected: {}\n      actual:   {}",
                index + 1,
                expected[index],
                actual[index]
            ));
        }
    }
    if expected.len() != actual.len() {
        return Some(format!(
            "    line count differs: expected {} line(s), got {}",
            expected.len(),
            actual.len()
        ));
    }
    None
}

/// 漂流の測定の既定の長さ（分）。**S6 の設計で先に固定した数である。**
const DRIFT_TEST_DEFAULT_MINUTES: u64 = 10;

/// 標本の間隔（ハートビート何本ごとか）。ハートビートは 1 秒に 1 本なので 10 秒。
const DRIFT_SAMPLE_STRIDE: usize = 10;

/// 漂流の測定（S6-c）。
///
/// # 何を主張するか
///
/// **定常状態で動かないはずの量が、実際に動かないこと。** 判定は
/// **「全標本が同一であること」**である。**傾きの推定はしない**——量は厳密な
/// 整数なので、揺れない。**多点の価値は「いつ動いたか」が特定できることにある。**
///
/// # 専用の出力経路を作っていない
///
/// **既に BKL の内側で出ているハートビートの行へ相乗りする**（`heap_free=` と
/// `heap_blocks=`）。行を増やせば混線の機会が増えるので増やさない。
/// **間引きはこちら側で行う**——カーネルには新しい周期の定数を置かない。
///
/// # 覆う範囲と、覆わない範囲
///
/// **覆うのは漂流であって、デッドロックではない。** BKL の待ちは
/// `WAIT_TIMEOUT_CYCLES`（実測で約 15 秒）で自分から報せて止まるので、
/// 10 分を要しない。
///
/// **10 分は導出値ではなく標本の予算である。** 10 分より長い周期で起きる事象は
/// 見えない。**フレームアロケータの量は測っていない**（下記）。
fn cmd_drift_test(minutes: u64, smp: Option<u32>) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel(&workspace_root, false)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root.join("target").join("drift-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });
    if let Some(count) = smp {
        qemu_args.push("-smp".into());
        qemu_args.push(count.to_string().into());
    }

    let cores = smp.map_or("default".to_string(), |c| c.to_string());
    println!("=== drift test: {minutes} minute(s), -smp {cores}, sampling every {DRIFT_SAMPLE_STRIDE} heartbeat(s)");

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the drift test")?;

    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_secs(5));
    }
    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start("drift test", firmware_rip, qemu_exit.as_deref());
    }

    let heartbeats: Vec<&str> = serial
        .lines()
        .filter(|l| l.contains("heartbeat: ticks="))
        .collect();
    let samples: Vec<&&str> = heartbeats.iter().step_by(DRIFT_SAMPLE_STRIDE).collect();
    println!(
        "--- drift test: {} heartbeat(s), {} sample(s)",
        heartbeats.len(),
        samples.len()
    );

    // **動かないはずの量。** 増える量（ティック等）はここに入れない。
    const INVARIANTS: &[&str] = &["heap_free=", "heap_blocks="];
    let mut failed = false;
    for field in INVARIANTS {
        let values: Vec<String> = samples
            .iter()
            .filter_map(|line| field_value(line, field))
            .collect();
        if values.len() != samples.len() {
            println!(
                "--- drift test: {field} missing from {} of {} sample(s): FAILED",
                samples.len() - values.len(),
                samples.len()
            );
            failed = true;
            continue;
        }
        let first = &values[0];
        match values.iter().position(|v| v != first) {
            None => println!(
                "--- drift test: {field}{first} identical across all {} sample(s): OK",
                values.len()
            ),
            Some(index) => {
                println!(
                    "--- drift test: {field} changed at sample {index} ({first} -> {}): FAILED",
                    values[index]
                );
                failed = true;
            }
        }
    }

    // **停止していないこと。** 動かない量が動かないだけでは足りない
    // （止まっていても動かない）。**進んでいる量が進んでいることを併せて見る。**
    let last_ticks = samples.last().and_then(|l| field_value(l, "ticks="));
    let first_ticks = samples.first().and_then(|l| field_value(l, "ticks="));
    match (first_ticks, last_ticks) {
        (Some(a), Some(b)) if a != b => println!("--- drift test: ticks advanced {a} -> {b}: OK"),
        (Some(a), Some(b)) => {
            println!("--- drift test: ticks did not advance ({a} -> {b}): FAILED");
            failed = true;
        }
        _ => {
            println!("--- drift test: could not read ticks from the samples: FAILED");
            failed = true;
        }
    }

    if failed {
        bail!("drift test: FAILED");
    }
    println!("drift test: PASS");
    Ok(())
}

/// ハートビートの行から `field=` に続く値を取り出す（区切りは空白かカンマ）。
fn field_value(line: &str, field: &str) -> Option<String> {
    let start = line.find(field)? + field.len();
    let rest = &line[start..];
    let end = rest.find([',', ' ']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn cmd_marker_test(
    tests: &[CriticalTest],
    kind_label: &str,
    kind: &str,
    smp: Option<u32>,
) -> Result<()> {
    let test = tests.iter().find(|t| t.name == kind).with_context(|| {
        let names: Vec<&str> = tests.iter().map(|t| t.name).collect();
        format!(
            "unknown {kind_label} {kind:?} (expected one of: {})",
            names.join(", ")
        )
    })?;

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let features: Vec<&str> = test.feature.split(',').collect();
    let kernel_elf = build_kernel_with_features(&workspace_root, &features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join(format!("{kind_label}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });
    // コア数を指定する構成（現在は ACPI の列挙が `-smp` と一致することの確認だけ）。
    // **既定は指定しない。** `QemuLaunchOptions` へ足さずここで付けるのは、
    // 他の起動経路の引数を一切変えないためである。
    if let Some(count) = smp {
        qemu_args.push("-smp".into());
        qemu_args.push(count.to_string().into());
    }

    // **期待マーカーが全部そろうまで待つ。** 先頭 1 本を見た時点で QEMU を
    // 落とす作りだと、残りのマーカーがまだシリアルへ流れている途中で
    // 打ち切られる。38400 baud では 4 行で約 70ms かかり、ポーリング間隔
    // （100ms）と同じ桁なので、たいていは間に合うが時々間に合わない。
    // 実際に `interrupt-test misaligned` が、4 行のうち 2 行だけ出た状態で
    // 「halting の行が無い」と判定して落ちた。テストの成否が実行ごとの
    // タイミングで変わる状態は、失敗を見ても実装の問題か取りこぼしかを
    // 区別できない。
    //
    // 全部そろうまで待てば、遅れて届く行を取りこぼさない。到達しない場合は
    // 従来どおり `deadline` で打ち切るので、上限は変わらない。
    let wait_for_full_timeout = test.wait_for_full_timeout;
    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the critical test")?;

    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    loop {
        if !wait_for_full_timeout && {
            let text = read_lossy(&serial_log);
            test.expected_markers.iter().all(|m| text.contains(m))
        } {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    // **kill する前に、既に終わっていないかを見る。** 自分から終了して
    // いたなら、それは QEMU 側の異常（引数が不正、OVMF が無い、ソケットを
    // 作れない等）であって、ゲストが動かなかったのとは別である。この 1 行が
    // 無いと両者を区別できない。
    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);

    let context = format!("{kind_label} {}", test.name);

    // **`tlb-generation` だけ、マーカーでは表せない関係を見る（S7-d）。**
    // マーカーは部分文字列の有無しか言えないので、**絶対値でしか書けない。**
    // この探りが主張しているのは関係のほうなので、ここで別に確かめる。
    let mut relation_note: Option<String> = None;
    if test.name == "tlb-generation" {
        match check_tlb_generation_relation(&serial) {
            Ok(note) => relation_note = Some(note),
            Err(error) => {
                println!("{context}: the generation relation does not hold: {error}");
                bail!("{context}: FAILED")
            }
        }
    }
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip, qemu_exit.as_deref());
    }

    println!("--- {context}: relevant output ---");
    for line in serial.lines().filter(|l| {
        l.contains("critical")
            || l.contains("lock:")
            || l.contains("sti")
            || l.contains("irq-path")
            || l.contains("heartbeat")
            || l.contains("interrupt-test")
            || l.contains("stack alignment")
            || l.contains("acpi")
    }) {
        println!("{line}");
    }
    println!("--- end ---");

    // 期待/禁止マーカーとは独立した、外側からの生存判定の結果。
    let mut ok_override = true;

    // **完全停止の外側からの検出。** メインループが `hlt` で眠ったまま
    // 二度と起きなくなった場合、カーネル自身はそれを検出できない（検出用の
    // コードが動かない）。QEMU も `-no-shutdown` で生き続けるため、プロセスの
    // 生死からも判断できない。**シリアルログのハートビート回数**だけが外から
    // 見える手掛かりになる。「カーネル内部では検出できないが、テスト基盤では
    // 検出できる」という切り分けである。
    if let Some(expected) = test.min_heartbeats {
        let seen = serial.matches("heartbeat: ticks=").count();
        println!("{context}: heartbeat lines = {seen} (expected at least {expected})");
        if seen < expected {
            println!(
                "{context}: too few heartbeats - the main loop probably stopped waking up. \
                 This is the only way to notice a permanent hlt from the outside."
            );
            ok_override = false;
        }
    }

    let mut ok = ok_override;
    for marker in test.expected_markers {
        let present = serial.contains(marker);
        ok &= present;
        println!(
            "{context}: serial contains {marker:?} = {}",
            if present { "OK" } else { "NG" }
        );
    }
    for marker in test.forbidden_markers {
        let absent = !serial.contains(marker);
        ok &= absent;
        println!(
            "{context}: serial does NOT contain {marker:?} = {}",
            if absent {
                "OK"
            } else {
                "NG (detection was bypassed)"
            }
        );
    }

    if ok {
        if let Some(note) = relation_note {
            println!("{context}: generation relation OK ({note})");
        }
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAIL")
    }
}

/// 例外ハンドラの回帰チェックを 1 種類実行する。
///
/// シリアルログのハンドラ出力と、`qemu-debug.log` の `v=..` の両方を
/// 突き合わせる。片方だけでは、ベクタ番号を取り違えたまま動いているように
/// 見える事故を防げない（ADR-0018）。
fn cmd_exception_test(kind: &str) -> Result<()> {
    let test = EXCEPTION_TESTS
        .iter()
        .find(|t| t.name == kind)
        .with_context(|| {
            let names: Vec<&str> = EXCEPTION_TESTS.iter().map(|t| t.name).collect();
            format!(
                "unknown exception test {kind:?} (expected one of: {})",
                names.join(", ")
            )
        })?;

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let features: Vec<&str> = test.feature.split(',').collect();
    let kernel_elf = build_kernel_with_features(&workspace_root, &features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("exception-test-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    // 例外の記録が要るので、この検証は常に TCG で行う。
    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let expected_serial = format!("[ERROR] exception: vector={} ", test.vector);
    // **ダンプの終端行が出るまで待つ。** 待ち条件を先頭行（vector=X）だけに
    // すると、それが現れた瞬間に kill してしまい、後続のダンプ（GPR・CR2・
    // on IST・halting）が serial へ書き出される前に切れることがある。実際に
    // double-fault で「on IST1=true」が捕捉から漏れて落ちた。ハンドラは必ず
    // 最後にこの行を出すので、これを見てから kill すれば全行がそろう
    // （マーカーテストの「全マーカーがそろうまで待つ」と同じ考え）。
    const DUMP_TERMINATOR: &str = "[ERROR] halting (cli + hlt loop)";

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the exception test")?;

    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let handler_ran = loop {
        let text = read_lossy(&serial_log);
        if text.contains(&expected_serial) && text.contains(DUMP_TERMINATOR) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    };

    // **kill する前に、既に終わっていないかを見る。** 自分から終了して
    // いたなら、それは QEMU 側の異常（引数が不正、OVMF が無い、ソケットを
    // 作れない等）であって、ゲストが動かなかったのとは別である。この 1 行が
    // 無いと両者を区別できない。
    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = read_lossy(&serial_log);
    let qemu = read_lossy(&debug_log);

    // テスト結果を読む前に、そもそもカーネルが起動したかを判定する。
    // 起動していなければ、以降の OK/NG は意味を持たない。
    let context = format!("exception-test {}", test.name);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip, qemu_exit.as_deref());
    }

    let qemu_saw_it = qemu.contains(test.qemu_marker);

    println!("--- exception-test {}: handler output ---", test.name);
    for line in serial.lines().filter(|l| l.starts_with("[ERROR]")) {
        println!("{line}");
    }
    println!("--- end ---");

    println!(
        "exception-test {}: handler reported vector {} = {}",
        test.name,
        test.vector,
        if handler_ran { "OK" } else { "NG" }
    );
    println!(
        "exception-test {}: qemu -d int recorded {} = {}",
        test.name,
        test.qemu_marker,
        if qemu_saw_it { "OK" } else { "NG" }
    );

    // 追加の目印（CR2、エラーコードの展開、IST の確認など）。
    let mut markers_ok = true;
    for marker in test.extra_serial_markers {
        let present = serial.contains(marker);
        markers_ok &= present;
        println!(
            "exception-test {}: serial contains {marker:?} = {}",
            test.name,
            if present { "OK" } else { "NG" }
        );
    }

    // 汎用レジスタの並び順。名前と値の対応が入れ替わっていれば落ちる。
    let mut registers_ok = true;
    if test.check_registers {
        let mut wrong = Vec::new();
        for (name, value) in KNOWN_REGISTERS {
            if !serial.contains(&format!("{name}={value}")) {
                wrong.push(*name);
            }
        }
        registers_ok = wrong.is_empty();
        if registers_ok {
            println!(
                "exception-test {}: all {} general purpose registers dumped with the \
                 expected value = OK",
                test.name,
                KNOWN_REGISTERS.len()
            );
        } else {
            println!(
                "exception-test {}: register mismatch for {:?} = NG (the push order in \
                 the stub and the field order in ExceptionContext disagree)",
                test.name, wrong
            );
        }
    }

    // トリプルフォルトになっていないこと。
    let mut reset_ok = true;
    if test.check_no_extra_cpu_reset {
        let resets = qemu.matches("CPU Reset").count();
        reset_ok = resets <= EXPECTED_CPU_RESET_COUNT;
        println!(
            "exception-test {}: CPU Reset count = {resets} (expected <= {}) = {}",
            test.name,
            EXPECTED_CPU_RESET_COUNT,
            if reset_ok { "OK" } else { "NG (triple fault?)" }
        );
    }

    if handler_ran && qemu_saw_it && markers_ok && registers_ok && reset_ok {
        println!("exception-test {}: PASS", test.name);
        Ok(())
    } else {
        bail!(
            "exception-test {}: FAIL (handler={handler_ran}, qemu={qemu_saw_it}, \
             markers={markers_ok}, registers={registers_ok}, cpu_reset={reset_ok})",
            test.name
        );
    }
}

/// 指定した feature 付きで kernel をビルドする。
/// `cargo xtask check` が順に実行する検査。
///
/// # なぜ 1 コマンドに畳むのか
///
/// M3-b 以降、実装ループから `cargo fmt --check` と `cargo clippy` が抜け落ち、
/// 誰も気づかないまま整形差分が 52 箇所、clippy 警告が 10 件まで積み上がった。
/// 原因は個々の見落としではなく、**維持していることを確認する手順が
/// どこにも無かった**ことである。手順が増えるほど飛ばされやすくなるので、
/// 覚えるものを 1 つに減らす。
///
/// # 構成を明示的に並べる理由
///
/// bootloader と kernel は**ターゲットが違う**（`x86_64-unknown-uefi` と
/// `x86_64-unknown-none`）。`--workspace --all-targets` でまとめて回すことは
/// できない。ホスト向けに bootloader をビルドしようとして失敗するためである。
/// 構成ごとに並べるほかない。
const CHECKS: &[(&str, &[&str])] = &[
    (
        "build bootloader (uefi)",
        &["build", "-p", BOOTLOADER_PACKAGE, "--target", UEFI_TARGET],
    ),
    (
        "build kernel (none)",
        &["build", "-p", KERNEL_PACKAGE, "--target", KERNEL_TARGET],
    ),
    ("build common (host)", &["build", "-p", "common"]),
    ("build xtask (host)", &["build", "-p", "xtask"]),
    ("test (host)", &["test", "--workspace"]),
    (
        "clippy bootloader (uefi)",
        &[
            "clippy",
            "-p",
            BOOTLOADER_PACKAGE,
            "--target",
            UEFI_TARGET,
            "--",
            "-D",
            "warnings",
        ],
    ),
    (
        "clippy kernel (none)",
        &[
            "clippy",
            "-p",
            KERNEL_PACKAGE,
            "--target",
            KERNEL_TARGET,
            "--",
            "-D",
            "warnings",
        ],
    ),
    // **ホストの 2 つは `--all-targets` を付ける。** **付けないと
    // `#[cfg(test)]` の中を一度も見ない**——**実測で、テストの中の
    // `duplicated attribute` を素通りさせていた**（2026-08-30。
    // **既定の形では警告 0、`--all-targets` では error 2 である**）。
    //
    // **`kernel` と `bootloader` には付けられない。** **`kernel` を
    // 素の標的で `--all-targets` すると 2,896 件出る**（テストの標的が
    // `x86_64-unknown-none` で建たない。実測）。**ホストの標的で見る道は
    // `build.rs` の生成物の在り処で落ちる**（実測）。**`deferred-decisions`
    // に行を立てた。**
    (
        "clippy common (host)",
        &[
            "clippy",
            "-p",
            "common",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    ),
    (
        "clippy xtask (host)",
        &[
            "clippy",
            "-p",
            "xtask",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    ),
    ("fmt --check", &["fmt", "--all", "--", "--check"]),
];

/// ホストテストの名前の集合を持つ参照。
const HOST_TEST_REFERENCE: &str = "xtask/reference/host-tests.txt";

/// ホストテストが走る単位。**`bootloader` も見る**（いまは 0 本だが、
/// **0 本であることも「消えた」の判定に要る**）。
const HOST_TEST_PACKAGES: &[&str] = &["common", "kernel", "xtask", "bootloader"];

/// 走るホストテストの名前を、パッケージ名を冠して並べる。
///
/// **パッケージ名を冠する理由は、名前が衝突するためである**——
/// `addr::tests::...` は `common` と `kernel` の両方に在りうる。
fn collect_host_test_names(workspace_root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for package in HOST_TEST_PACKAGES {
        let output = Command::new("cargo")
            .current_dir(workspace_root)
            .args(["test", "-p", package, "--", "--list"])
            .output()
            .with_context(|| format!("failed to list the host tests of {package}"))?;
        if !output.status.success() {
            bail!("cargo test -p {package} -- --list did not succeed");
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(name) = line.strip_suffix(": test") {
                names.push(format!("{package}::{name}"));
            }
        }
    }
    // **並べ替えるが、重複は落とさない。** **同じ名前が 2 つ在ることも
    // 見えたほうがよい。**
    names.sort();
    Ok(names)
}

/// 走るホストテストの名前の集合を、参照と突き合わせる。
///
/// # なぜ本数ではなく名前なのか
///
/// **本数は相殺に弱い。** **実測で踏んだ**——**1 本が走らなくなった同じ
/// コミットで 1 本足しており、合計が動かなかった**（2026-08-30。
/// `docs/verification-coverage.md`）。**名前の集合なら、消えた名前が
/// そのまま出る。**
///
/// # なぜ主張にしたのか（報告に留めなかったのか）
///
/// **取り直す手間を実測で比べた。** **`#[test]` を触ったコミットは 88 本、
/// 起動ログの参照を取り直したコミットは 131 本である**（全 751 本のうち。
/// 2026-08-30）。**既に払っている手間より軽い。**
///
/// # clippy と二重に持つ
///
/// **`--all-targets` を付けた clippy は、今回の形（属性が外れる）を
/// 捕まえる。** **それでもこちらを持つ**——**捕まえる範囲が違う。**
/// **`#[ignore]` を付ける・`cfg` の裏へ入る・丸ごと消す、は
/// lint に出ない。** **「走る集合」を直接見るのはこちらだけである。**
fn check_host_test_names(workspace_root: &Path, update: bool) -> Result<String> {
    let names = collect_host_test_names(workspace_root)?;
    let reference = workspace_root.join(HOST_TEST_REFERENCE);
    let recorded = format!("{}\n", names.join("\n"));
    if update {
        if let Some(parent) = reference.parent() {
            fs::create_dir_all(parent).context("failed to create the reference directory")?;
        }
        fs::write(&reference, &recorded).context("failed to record the host test names")?;
        return Ok(format!(
            "reference updated ({} name(s)) at {HOST_TEST_REFERENCE}",
            names.len()
        ));
    }
    let Ok(expected) = fs::read_to_string(&reference) else {
        bail!(
            "{HOST_TEST_REFERENCE} is missing; run `cargo xtask check --update-reference` once \
             to record it"
        )
    };
    let before: Vec<&str> = expected.lines().collect();
    let after: Vec<&str> = names.iter().map(String::as_str).collect();
    if before == after {
        return Ok(format!("OK ({} name(s))", names.len()));
    }
    let gone: Vec<&&str> = before.iter().filter(|n| !after.contains(n)).collect();
    let fresh: Vec<&&str> = after.iter().filter(|n| !before.contains(n)).collect();
    for name in &gone {
        println!("    gone:  {name}");
    }
    for name in &fresh {
        println!("    new:   {name}");
    }
    bail!(
        "{} name(s) gone and {} new; if the change is intended, re-record with \
         `cargo xtask check --update-reference` and say so in the commit",
        gone.len(),
        fresh.len()
    )
}

/// 直接の割り込み制御（`InterruptGuard` 非経由の `cli`/`sti`）を許可する箇所。
///
/// 排他は `common::critical` の [`InterruptGuard`]/`Locked<T>` の裏に閉じる決まりで
/// ある（ADR-0023 §3 の seam整備）。それでも直接 `cli`/`sti` が要る箇所は存在し、
/// **いずれも「共有データの排他」以外の目的**である。目的別に許可し、リスト外の
/// 出現は FAIL させる。
///
/// # 行番号ではなく所属アイテム名で識別する
///
/// 行番号は編集のたびに動くので使わない。所属する関数名（`global_asm!` の中は
/// `global_asm!`）で識別する。**関数名を変えると検査が落ちる**が、これは欠点では
/// なく利点である。許可リストは「意図的に承認したもの」の記録なので、コードが
/// 変わったら再承認を求めるのが正しい。
///
/// # この検査が守れないこと
///
/// 検出できるのは「新しい直接 `cli`/`sti` の追加」だけである。**許可済み箇所の
/// 中身が排他目的に変質したことは検出できない**（文字列走査では意図は見えない）。
struct DirectInterruptControlSite {
    /// ワークスペース相対パス。
    file: &'static str,
    /// 所属する関数名。`global_asm!` の中は `"global_asm!"`。
    item: &'static str,
    /// なぜ直接触ってよいのか。
    reason: &'static str,
}

/// シリアルへの口を直接開けてよい箇所（[`DIRECT_SERIAL_PORT_ALLOWLIST`] 参照）。
struct DirectSerialPortSite {
    /// ワークスペース相対のパス。
    file: &'static str,
    /// その中の所属名（関数名）。
    item: &'static str,
    /// なぜ BKL の外から書いてよいのか。
    reason: &'static str,
}

/// シリアルへの口を直接開けてよい箇所の許可リスト（S6-b）。
///
/// # 名前は、見ているものを指す
///
/// **動機は「BKL の外から書いてよい箇所を固定する」だが、実際に見ているのは
/// `SerialPort::new(` を書いた箇所である。** 書き込みでも BKL の状態でもない。
/// **守りたいことではなく、見ているものを名前にしてある**（同じずれを 3 度
/// 起こしている。`verification-coverage.md` の失敗類型）。
///
/// # 何を固定していて、何を固定していないか
///
/// **固定するのは「どこから書いてよいか」だけである。** **許可された箇所どうしの
/// 混線は防げない**——S4-b-4 で、BKL の外の 2 行がバイト単位で混ざる実物を観測して
/// いる（`docs/verification-coverage.md`）。**この許可リストはその範囲を狭めるもので
/// あって、混線を無くすものではない。**
///
/// # 粒度の限界
///
/// **見ているのは `SerialPort::new(` を書いた箇所である。** `kernel/src/task.rs` の
/// `serial_line` のように**多数の呼び出しをまとめる出口**があると、**1 エントリが
/// その全部を覆う**（実測で 31 箇所）。**「どのファイルのどの関数が口を開けるか」は
/// 固定できるが、「その口を誰が使うか」は固定できない。**
///
/// # bootloader が対象外である理由
///
/// **bootloader には BKL が存在しない。** 「BKL の外」という区別自体が無いので、
/// 許可リストの意味が無い。**対象は `kernel/` と `common/` である。**
const DIRECT_SERIAL_PORT_ALLOWLIST: &[DirectSerialPortSite] = &[
    // (a) BKL 自身。**取れない**——ここで取ると、報せようとしている当の対象を
    // もう一度取ることになる。
    DirectSerialPortSite {
        file: "kernel/src/bkl.rs",
        item: "report_recursive_acquire_and_halt",
        reason: "BKL の再帰取得の報告。BKL を取れない当の経路である",
    },
    DirectSerialPortSite {
        file: "kernel/src/bkl.rs",
        item: "report_timeout_and_halt",
        reason: "BKL の待ちがタイムアウトした報告。取れないから報せている",
    },
    DirectSerialPortSite {
        file: "kernel/src/bkl.rs",
        item: "sabotage_hold_forever",
        reason: "破壊 bkl-hold-forever-test の実装。保持したまま報せる",
    },
    DirectSerialPortSite {
        file: "kernel/src/bkl.rs",
        item: "sabotage_enable_interrupts_while_held",
        reason: "破壊 bkl-hold-with-if-set-test の実装。保持したまま報せる",
    },
    DirectSerialPortSite {
        file: "kernel/src/bkl.rs",
        item: "report_sabotage_did_not_fire_and_halt",
        reason: "破壊が発火しなかったことの報告。保持したまま停止する経路である",
    },
    // (b) 戻らない経路。**ADR-0023 で BKL を取らないと決めてある。**
    // 取らないと決めた以上、その経路の行も BKL の外にしかなりえない。
    DirectSerialPortSite {
        file: "kernel/src/panic.rs",
        item: "panic",
        reason: "パニックハンドラ（ADR-0004 の halt and dump、ADR-0023 で BKL を取らない）",
    },
    DirectSerialPortSite {
        file: "kernel/src/idt/mod.rs",
        item: "exception_entry",
        reason: "例外ハンドラ。依存を最小にする（ADR-0018）",
    },
    DirectSerialPortSite {
        file: "kernel/src/idt/mod.rs",
        item: "check_stack_alignment",
        reason: "スタブ入口の境界違反の報告。違反した状態で呼び出しを増やさない",
    },
    DirectSerialPortSite {
        file: "common/src/critical.rs",
        item: "report_contended_lock_and_halt",
        reason: "Locked<T> の競合の報告。停止する経路である",
    },
    DirectSerialPortSite {
        file: "common/src/critical.rs",
        item: "report_double_lock_and_halt",
        reason: "Locked<T> の二重取得の報告。停止する経路である",
    },
    DirectSerialPortSite {
        file: "kernel/src/console/screen.rs",
        item: "report_flush_failure_and_halt",
        reason: "画面への書き出しが失敗したときの報告。停止する経路である",
    },
    // (c) BKL より下位の機構。**BKL がこれらに依存しているので、逆向きに
    // 依存させられない。**
    DirectSerialPortSite {
        file: "kernel/src/main.rs",
        item: "kernel_main",
        reason: "ロガーを組み立てる起点。BKL はまだ無い",
    },
    DirectSerialPortSite {
        file: "kernel/src/gdt/mod.rs",
        item: "cpu_id_from_gdtr",
        reason: "cpu_id() 自身の失敗経路。BKL は cpu_id に依存する",
    },
    DirectSerialPortSite {
        file: "kernel/src/heap/allocator.rs",
        item: "log_directly_to_serial",
        reason: "確保に失敗したときの報告。ヒープ経由のログは再帰的に確保しうる",
    },
    // (d) AP の起動経路。**BKL に参加する前である。**
    DirectSerialPortSite {
        file: "kernel/src/smp.rs",
        item: "zaytos_ap_entry",
        reason: "AP の入口。per-CPU もスタックもまだ整っていない",
    },
    DirectSerialPortSite {
        file: "kernel/src/smp.rs",
        item: "bring_up_application_processor",
        reason: "AP の起こし。BKL へ参加する前の経過を出す",
    },
    DirectSerialPortSite {
        file: "kernel/src/smp.rs",
        item: "ap_after_switch",
        reason: "AP がスタックを切り替えた直後。BKL を取る区間は書き込みだけに絞ってある",
    },
    // (e) 同時進入の報告。**BKL の外にいることを報せる行なので、取れない。**
    DirectSerialPortSite {
        file: "kernel/src/idt/mod.rs",
        item: "report_concurrent_entry_once",
        reason: "カーネル内の同時進入の報告。BKL の外にいることが報告内容である",
    },
    // (f) **ここだけ性質が違う。**
    //
    // `serial_line` は**スケジューラの観測行の出口**で、31 箇所から呼ばれる
    // （S6-b で数えた）。**呼び出し元は BKL の内側と外側にまたがっている**——
    // `schedule_switch` や検出器は内側、デモの進行を出す行は外側である。
    //
    // **したがってこの 1 エントリは「BKL の外から書いてよい」を主張していない。**
    // 主張しているのは「シリアルへの口をここに 1 つだけ開ける」である。
    // **許可リストの粒度がこの出口までしか届かないことが、この検査の限界である**
    // （型の doc の「粒度の限界」）。
    DirectSerialPortSite {
        file: "kernel/src/syscall.rs",
        item: "sys_write",
        reason:
            "Ring 3 の write を届ける先。BKL の内側だが、ロガーもコンソールも lib からは届かない",
    },
    DirectSerialPortSite {
        file: "kernel/src/syscall.rs",
        item: "ioctl_log_line",
        reason: "診断の出口（ADR-0046）。画面へ出さずログへ出す当の経路である",
    },
    DirectSerialPortSite {
        file: "kernel/src/console/probe.rs",
        item: "observe",
        reason: "画面の観測の判定行（ES-d）。sys_write と同じで、lib からロガーへ届かない",
    },
    DirectSerialPortSite {
        file: "kernel/src/console/mod.rs",
        item: "report_dropped_mid_character",
        reason: "字の途中で捨てたバイトの報告（ADR-0054）。probe::observe と同じで、lib からロガーへ届かない",
    },
    DirectSerialPortSite {
        file: "kernel/src/userland.rs",
        item: "spawn",
        reason: "spawn の判定行。BKL を解いた区間で走る（ADR-0023 §1）ので、ロガーを渡す道が無い",
    },
    DirectSerialPortSite {
        file: "kernel/src/task.rs",
        item: "serial_line",
        reason: "スケジューラの観測行の唯一の出口（31 箇所の呼び出しを覆う。粒度の限界）",
    },
];

/// 直接 `cli`/`sti` の許可リスト（[`DirectInterruptControlSite`] 参照）。
const DIRECT_INTERRUPT_CONTROL_ALLOWLIST: &[DirectInterruptControlSite] = &[
    // (a) 排他の実装本体。ここが「排他の所在」であり、他は全部これを使う。
    DirectInterruptControlSite {
        file: "common/src/critical.rs",
        item: "enter",
        reason: "InterruptGuard::enter そのもの（排他の実装本体）",
    },
    DirectInterruptControlSite {
        file: "common/src/critical.rs",
        item: "drop",
        reason: "InterruptGuard::drop の復元（排他の実装本体）",
    },
    // (b) 起動の一度きり。スコープを抜けたら復元する意味を持たない恒久的な禁止。
    DirectInterruptControlSite {
        file: "kernel/src/main.rs",
        item: "_start",
        reason: "M4-d まで恒久的に禁止する起動時の一度きり（ADR-0014）",
    },
    // (c)(d)(e) 割り込み許可状態の遷移。sti は検証 7 項目通過後のみ（ADR-0018 §2）。
    DirectInterruptControlSite {
        file: "kernel/src/interrupts.rs",
        item: "spin_with_interrupts_enabled",
        reason: "sti する箇所の 1 つ（M4-d-1 の期限つきスピン）と観測後の復帰 cli",
    },
    DirectInterruptControlSite {
        file: "kernel/src/interrupts.rs",
        item: "run_timer_loop",
        reason: "sti する箇所の 1 つ（M4-d-2 のタイマループ）と sti;hlt 隣接・上限到達時の cli",
    },
    DirectInterruptControlSite {
        file: "kernel/src/virtio.rs",
        item: "exercise_blocking_read",
        reason: "I/O 待ちの sti;hlt 隣接（S13-d-2。ADR-0036）。cli 下で完了を検査し、                 未完了なら enable_interrupts_and_halt で眠る",
    },
    DirectInterruptControlSite {
        file: "kernel/src/virtio.rs",
        item: "wait_for_image_write",
        reason: "I/O 待ちの sti;hlt 隣接（P-c-1。ADR-0036）。exercise_blocking_read と \
                 同じ形で、こちらはシェルの文脈から呼ばれる本番の利用者である。cli 下で \
                 完了を検査し、未完了なら enable_interrupts_and_halt で眠る。BKL は \
                 呼び出し側が解いてある",
    },
    DirectInterruptControlSite {
        file: "kernel/src/smp.rs",
        item: "ap_heartbeat_loop",
        reason: "AP の定常ループ（S4-a）。sti;hlt 隣接で、BSP の run_timer_loop と同じ形で \
                 ある。**排他ではない。** この段の AP はタスクを実行せず、ハンドラが触る \
                 のは per-CPU かアトミックだけである（roadmap.md の S4-a）。**BKL が \
                 入る S4-b で、この一覧に依存した安全は不要になる。**",
    },
    // (h) BKL の排他（S4-b-2）。**自発的なクリティカルセクションではないので
    // 深さを数えない。** 数えると `on_timer_tick` の防御スキップと `on_yield` の
    // 判定が壊れる（`EntryInterruptGuard` の doc）。
    // **`EntryInterruptGuard::enter` と `::drop` のエントリはここにあった。**
    // **S6-d の死んだエントリの検査が見つけて外した。** 走査が作る所属名は
    // **裸の関数名**（`function_name_declared_on`）なので、**`型::メソッド` の
    // 書き方は一致しようがない。1 度も効いていなかった。**
    //
    // **実体は上の `enter` / `drop` の 2 件が覆っている**（同じファイルの同じ
    // 関数名なので、`InterruptGuard` と `EntryInterruptGuard` を**区別できない**）。
    // **これは粒度の限界である**——`serial_line` の 1 エントリが 31 箇所を覆うのと
    // 同じ形で、`docs/verification-coverage.md` に書いてある。
    DirectInterruptControlSite {
        file: "kernel/src/bkl.rs",
        item: "sabotage_enable_interrupts_while_held",
        reason: "破壊 feature 専用（bkl-hold-with-if-set）。**保持区間 = IF=0 の \
                 不変条件そのものを壊す。** 既定ビルドには存在しない",
    },
    // (f) テスト経路。復元経路そのものを実証するので直接触る必要がある。
    DirectInterruptControlSite {
        file: "kernel/src/main.rs",
        item: "trigger_critical_test",
        reason: "IF=1 で enter した場合の復元経路を実証する検査（critical-test）",
    },
    // (g) **例外: ここは共有データの排他である。** InterruptGuard を使えない asm 文脈
    // なので生の cli/sti で守っている。**BKL では同一コアの割り込みしか防げないため
    // 再検討が要る**（deferred-decisions.md の「per-CPU seam が MAX_CPUS > 1 で…」の
    // 隣に論点として記録した）。
    DirectInterruptControlSite {
        file: "kernel/src/smp.rs",
        item: "global_asm!",
        reason: "AP トランポリンの入口。**排他ではない。** SIPI 直後の AP は \
                 リアルモードで IDT を持たないので、割り込みが来ても行き先が無い。 \
                 InterruptGuard は 16 ビットの asm からは使えない。**対応する sti が \
                 無いのは設計である**（AP は IDT を載せずに halt するので、割り込みを \
                 有効化する地点が存在しない）。この検査を将来「cli と sti が対になって \
                 いること」へ強化するなら、**このエントリは意図的な例外として扱うこと。**",
    },
    DirectInterruptControlSite {
        file: "kernel/src/task.rs",
        item: "global_asm!",
        reason: "GPR_BUF（A/B 共有）の store と照合を守る排他。asm 文脈で InterruptGuard を \
                 使えないための例外。BKL で再検討（複数コアでは防げない）",
    },
];

/// 許可リストに無い直接の割り込み制御を探す。
///
/// 対象は `cpu::disable_interrupts` / `cpu::enable_interrupts`
/// （`enable_interrupts_and_halt` を含む）の呼び出しと、`asm!`/`global_asm!` 内の
/// 生の `"cli"` / `"sti"`。
///
/// `common/src/cpu.rs` は除外する（primitive の定義本体で、命令そのものはここに
/// 集約されている）。`halt_forever` の `cli; hlt` も同ファイルなので自動的に外れる
/// （停止用であって排他ではない）。
/// # 数の単位
///
/// 許可リストのエントリ数（`file` + `item` の組）と、実際の**出現数**（`cli`/`sti` を
/// 含む行数）は別の単位である。1 つの関数に複数の出現があれば 1 エントリで複数
/// 出現になる（例: `run_timer_loop` は `sti` / `sti;hlt` / 上限到達時の `cli`）。
/// 数字を並べたときに取り違えないよう、`approved_occurrences` で出現数を数えて
/// 呼び出し側が両方を表示できるようにする。
fn find_unapproved_interrupt_control(
    workspace_root: &Path,
    approved_occurrences: &mut usize,
) -> Result<Vec<String>> {
    // **死んだエントリも探す（S6-d）。** 一致しなかったエントリが残っていると、
    // **検査は緑のまま通り、一覧を読んだ人は「この箇所は許可されている」と読む。**
    // **一覧が静かに狭くなることの鏡像である。**
    let mut used = vec![false; DIRECT_INTERRUPT_CONTROL_ALLOWLIST.len()];
    // SAFETY 検査と同じ理由で、追跡済みだけでなく未追跡のファイルも見る
    // （新規ファイルの最初の検査が素通りするのを防ぐ）。
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "*.rs",
        ])
        .output()
        .context("failed to list Rust sources")?;
    if !output.status.success() {
        bail!("git ls-files failed while collecting Rust sources");
    }
    let listing = String::from_utf8(output.stdout).context("git ls-files produced non-UTF-8")?;

    let mut findings = Vec::new();
    for relative in listing.lines().filter(|l| !l.is_empty()) {
        // primitive の定義本体は対象外。
        if relative == "common/src/cpu.rs" {
            continue;
        }
        // xtask はホスト上のビルドツールで、ring 0 の命令を実行しえない。この
        // 検査自身の実装（needle の文字列リテラルを含む）もここに入る。
        if relative.starts_with("xtask/") {
            continue;
        }
        let path = workspace_root.join(relative);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let mut current_item = "<file scope>";
        let mut in_global_asm = false;
        // `asm!` / `global_asm!` の**塊の中にいるか**。生の命令は `asm!(` と別の行に
        // 書かれるので、塊全体を追う必要がある（`asm!` を含む行だけを見ていたときは
        // 複数行の `asm!` 内の `cli` を取りこぼした）。開き `asm!(` から、trim 後に
        // `);` で始まる行までを塊とする。
        //
        // **同じ行で閉じる呼び出し（`asm!("cli")` の 1 行形）では塊に入れない。**
        // 入れてしまうと閉じ `);` が独立した行に現れないため `in_asm` が解除されず、
        // **そのファイルの残り全部を asm の中と誤認する**。実測では、単一行の
        // `global_asm!` を 1 つ置くだけで `current_item` が固定され、`interrupts.rs` の
        // 許可済み 5 箇所すべてが所属名の不一致で未許可と判定された（FAIL する方向
        // なので静かには壊れないが、正当な 1 行形を書くと検査が使えなくなる）。
        let mut in_asm = false;
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            // この行が `asm!` 呼び出しを開いているか（1 行で閉じる形を含む）。
            let opens_asm = line.contains("asm!(");
            let closes_on_same_line = opens_asm && asm_call_closes_on_same_line(line);

            if opens_asm {
                if !closes_on_same_line {
                    in_asm = true;
                    if line.contains("global_asm!(") {
                        in_global_asm = true;
                        current_item = "global_asm!";
                    }
                }
            } else if in_asm && trimmed.starts_with(");") {
                in_asm = false;
                if in_global_asm {
                    in_global_asm = false;
                    current_item = "<file scope>";
                }
            } else if !in_global_asm {
                if let Some(name) = function_name_declared_on(line) {
                    current_item = name;
                }
            }

            // ドキュメントコメントと行コメントは対象外（説明文で言及するのは自由）。
            if trimmed.starts_with("///") || trimmed.starts_with("//!") || trimmed.starts_with("//")
            {
                continue;
            }

            // 1 行で閉じる形は `in_asm` に入れないので、その行自体は `opens_asm` で拾う。
            let hit = (in_asm || opens_asm) && mentions_raw_instruction(line)
                || mentions_interrupt_primitive(line);

            if !hit {
                continue;
            }

            let matched = DIRECT_INTERRUPT_CONTROL_ALLOWLIST
                .iter()
                .position(|site| site.file == relative && site.item == current_item);
            if let Some(index) = matched {
                *approved_occurrences += 1;
                used[index] = true;
                continue;
            }
            findings.push(format!(
                "{relative}:{} (in {current_item}): {}",
                index + 1,
                line.trim().chars().take(60).collect::<String>()
            ));
        }
    }
    for (index, hit) in used.iter().enumerate() {
        if !hit {
            let site = &DIRECT_INTERRUPT_CONTROL_ALLOWLIST[index];
            findings.push(format!(
                "dead allowlist entry (nothing matched): {} / {} / {}",
                site.file, site.item, site.reason
            ));
        }
    }
    Ok(findings)
}

/// 許可リストに無い場所でシリアルの口を開けている箇所を探す（S6-b）。
///
/// **`cli`/`sti` の走査と同じ形にしてある**（同じ `function_name_declared_on` で
/// 所属名を追い、コメント行を飛ばし、追跡済みと未追跡の両方を見る）。
/// **`asm!` の塊を追う必要はない**——シリアルは Rust の式でしか触らない。
/// 出力を解析する外の道具を、直に `Command::new` している箇所を探す
/// （e-4 の後の手当て）。
///
/// # 何を見ているか
///
/// **`Command::new("<道具>")` と書いた行である**（[`PARSED_EXTERNAL_TOOLS`]）。
/// **`external_tool` を通せば言語が固定される**ので、**直に書いた箇所だけが
/// 環境の言語で答えを受ける。**
///
/// # 何を見ていないか
///
/// **道具の一覧に無いものは見ない。** **新しく解析する道具を足したら、
/// 一覧へも足すこと**——**この検査は「一覧に載っているものが寄せてあるか」
/// しか言わない**（列挙で守る検査の限界。`verification-coverage.md`）。
///
/// **`external_tool` 自身は数えない**（あそこが唯一の `Command::new` である）。
fn find_direct_external_tool_calls(workspace_root: &Path) -> Result<Vec<String>> {
    let mut findings = Vec::new();
    for relative in ["xtask/src/main.rs", "kernel/build.rs"] {
        let path = workspace_root.join(relative);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for (number, line) in text.lines().enumerate() {
            for tool in PARSED_EXTERNAL_TOOLS {
                let direct = format!("Command::new(\"{tool}\")");
                if line.contains(&direct) {
                    findings.push(format!(
                        "{relative}:{}: {tool} is called directly; go through external_tool() so \
                         LC_ALL=C is set",
                        number + 1
                    ));
                }
            }
        }
    }
    Ok(findings)
}

fn find_unapproved_direct_serial_ports(
    workspace_root: &Path,
    approved_occurrences: &mut usize,
) -> Result<Vec<String>> {
    // 死んだエントリも探す（`find_unapproved_interrupt_control` と同じ理由）。
    let mut used = vec![false; DIRECT_SERIAL_PORT_ALLOWLIST.len()];
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "*.rs",
        ])
        .output()
        .context("failed to list Rust sources")?;
    if !output.status.success() {
        bail!("git ls-files failed while collecting Rust sources");
    }
    let listing = String::from_utf8(output.stdout).context("git ls-files produced non-UTF-8")?;

    let mut findings = Vec::new();
    for relative in listing.lines().filter(|l| !l.is_empty()) {
        // 対象は kernel と common だけ（許可リストの doc の「bootloader が対象外で
        // ある理由」）。xtask はホスト側で、この検査自身の文字列リテラルも入る。
        if !(relative.starts_with("kernel/") || relative.starts_with("common/")) {
            continue;
        }
        // ポートの実装本体は対象外。
        if relative == "common/src/serial.rs" {
            continue;
        }
        let path = workspace_root.join(relative);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let mut current_item = "<file scope>";
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(name) = function_name_declared_on(line) {
                current_item = name;
            }
            if trimmed.starts_with("///") || trimmed.starts_with("//!") || trimmed.starts_with("//")
            {
                continue;
            }
            if !line.contains("SerialPort::new(") {
                continue;
            }
            let matched = DIRECT_SERIAL_PORT_ALLOWLIST
                .iter()
                .position(|site| site.file == relative && site.item == current_item);
            if let Some(index) = matched {
                *approved_occurrences += 1;
                used[index] = true;
                continue;
            }
            findings.push(format!(
                "{relative}:{} (in {current_item}): {}",
                index + 1,
                line.trim().chars().take(60).collect::<String>()
            ));
        }
    }
    for (index, hit) in used.iter().enumerate() {
        if !hit {
            let site = &DIRECT_SERIAL_PORT_ALLOWLIST[index];
            findings.push(format!(
                "dead allowlist entry (nothing matched): {} / {} / {}",
                site.file, site.item, site.reason
            ));
        }
    }
    Ok(findings)
}

/// `asm!` 呼び出しがその行の中で閉じているか（`asm!("cli")` の 1 行形）。
///
/// `asm!(` の `(` から括弧を数え、その行の終わりで残高が 0 なら閉じている。
/// 文字列リテラルの中の括弧は数えない（asm のオペランド文字列に `(` が現れても
/// 残高を狂わせないため）。
///
/// # 守れない範囲
///
/// - `line.find("asm!(")` は**最初の 1 つ**しか見ない。1 行に複数の `asm!` 呼び出しが
///   あると 2 つ目以降を見落とす。
/// - 文字列内のエスケープ `\"` で `in_string` が誤って反転する。
///
/// どちらも現在のコードには該当箇所が無く実害は無い。これらは
/// `docs/verification-coverage.md` に挙げた「検出をブロック追跡から切り離す」提案の
/// 根拠 3 件目でもある（その場しのぎのスキャナでソースを解析していることが原因）。
fn asm_call_closes_on_same_line(line: &str) -> bool {
    let Some(position) = line.find("asm!(") else {
        return false;
    };
    let mut depth = 0i32;
    let mut in_string = false;
    for character in line[position + "asm!".len()..].chars() {
        match character {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// その行の文字列リテラルが生の `cli` / `sti` 命令を含むか（`asm!` の中）。
///
/// # 引用符の内側の空白まで見る必要がある
///
/// asm のオペランドは `"  cli",` のように**引用符の内側にインデントを付けて**
/// 書かれる。当初 `"cli"` の完全一致で探していたため、`kernel/src/task.rs` の
/// `global_asm!` にある `"  cli"` / `"  sti"`（GPR_BUF を守る唯一の「排他目的」の
/// 直接操作）を**取りこぼしていた**。許可リストに載っているのに検出されない、
/// つまりその許可エントリが死んでいて、同じ書き方で新しく追加された `cli` も
/// 素通りする状態だった。
///
/// そこで各文字列リテラルを取り出して trim し、先頭トークン（`;` `,` を除く）が
/// `cli` / `sti` かで判定する。`"sti-check 1: ..."` のようなログ文字列は先頭
/// トークンが `sti-check` になるので一致しない。
fn mentions_raw_instruction(line: &str) -> bool {
    line.split('"').skip(1).step_by(2).any(|literal| {
        literal
            .trim()
            .split(|c: char| c.is_whitespace() || c == ';' || c == ',')
            .next()
            .is_some_and(|token| token == "cli" || token == "sti")
    })
}

/// その行が割り込み制御 primitive を呼んでいるか。
///
/// `may_enable_interrupts` のような別の識別子の一部を拾わないよう、直前の文字が
/// 識別子構成文字でないことを確かめる。
fn mentions_interrupt_primitive(line: &str) -> bool {
    ["disable_interrupts", "enable_interrupts"]
        .iter()
        .any(|needle| {
            let mut rest = line;
            let mut base = 0usize;
            while let Some(position) = rest.find(needle) {
                let absolute = base + position;
                let preceded_by_identifier = line[..absolute]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                if !preceded_by_identifier {
                    return true;
                }
                base = absolute + needle.len();
                rest = &line[base..];
            }
            false
        })
}

/// その行が宣言している関数名（`fn` の直後の識別子）。宣言でなければ `None`。
fn function_name_declared_on(line: &str) -> Option<&str> {
    let position = line.find("fn ")?;
    // `fn` の前が識別子構成文字なら別の語の一部（`asm_fn` など）。
    if line[..position]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let rest = line[position + "fn ".len()..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let name = &rest[..end];
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// `// SAFETY:` コメントを伴わない `unsafe` ブロックを探す。
///
/// `unsafe` ブロックには、なぜ safe に書けないのかと、何を前提に安全性が
/// 保たれるのか（invariant）を `// SAFETY:` コメントとして添える決まりで
/// ある。
///
/// # 走査の規則
///
/// `unsafe {` の行から上へ遡り、**最初に現れる非空行**を見る。それが `//`
/// で始まる行なら、そこから連続するコメント塊の中に `SAFETY` があるかを見る。
/// コメント塊でなければ、間に挟まってよいものだけを読み飛ばす。
///
/// 読み飛ばすのは属性（`#[...]`）と、`let ... =` のように `unsafe` ブロックを
/// 右辺に取る行の左側だけである。これらは SAFETY コメントと `unsafe` の間に
/// 正当に挟まりうる。それ以外の行に当たったら、コメントは無いものとする。
///
/// ドキュメントコメント（`///` と `//!`）の中の `unsafe {` は対象外。
/// 使用例を書いただけの行まで拾うと、直せない指摘が出続ける。
fn find_unsafe_without_safety_comment(workspace_root: &Path) -> Result<Vec<String>> {
    // **追跡済みだけでなく、未追跡のファイルも見る。**
    //
    // 以前は `git ls-files "*.rs"` で追跡済みだけを列挙していた。ところが
    // 新しいファイルを書いてから検査を通し、その後に `git add` して
    // コミットする、という自然な順序だと、**そのファイルの最初の検査は
    // 追跡される前に走る。** 素通りしたまま「通った」と報告され、次に
    // 検査が走るまで誰も気づかない。実際に `kernel/src/paging/verify.rs`
    // がこれで 2 コミットのあいだ見逃されていた。
    //
    // `--cached --others --exclude-standard` にすると、追跡済みと、
    // 無視されていない未追跡の両方が出る。`--exclude-standard` を付けるのは
    // `target/` の中を拾わないためである。
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "*.rs",
        ])
        .output()
        .context("failed to list Rust sources")?;
    if !output.status.success() {
        bail!("git ls-files failed while collecting Rust sources");
    }
    let listing = String::from_utf8(output.stdout).context("git ls-files produced non-UTF-8")?;

    let mut findings = Vec::new();
    for relative in listing.lines().filter(|l| !l.is_empty()) {
        let path = workspace_root.join(relative);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let lines: Vec<&str> = source.lines().collect();

        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("///") || trimmed.starts_with("//!") {
                continue;
            }
            if !contains_unsafe_block_opener(line) {
                continue;
            }
            if has_safety_comment_above(&lines, index) {
                continue;
            }
            findings.push(format!(
                "{relative}:{}: {}",
                index + 1,
                line.trim().chars().take(70).collect::<String>()
            ));
        }
    }
    Ok(findings)
}

/// その行が `unsafe` ブロックを開いているか。
///
/// `unsafe fn` の宣言は対象にしない。関数側の契約は `# Safety` セクションで
/// 書く決まりで、ブロックの `// SAFETY:` とは役割が違う。
fn contains_unsafe_block_opener(line: &str) -> bool {
    let mut rest = line;
    while let Some(position) = rest.find("unsafe") {
        let after = rest[position + "unsafe".len()..].trim_start();
        let before_is_boundary = position == 0
            || !rest[..position]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if before_is_boundary && after.starts_with('{') {
            return true;
        }
        rest = &rest[position + "unsafe".len()..];
    }
    false
}

fn has_safety_comment_above(lines: &[&str], index: usize) -> bool {
    let mut cursor = index;
    while cursor > 0 {
        cursor -= 1;
        let trimmed = lines[cursor].trim();
        if trimmed.is_empty() {
            return false;
        }
        if trimmed.starts_with("//") {
            // コメント塊に入った。塊を上へ辿って SAFETY を探す。
            let mut scan = cursor;
            loop {
                let text = lines[scan].trim();
                if !text.starts_with("//") {
                    return false;
                }
                if text.contains("SAFETY") {
                    return true;
                }
                if scan == 0 {
                    return false;
                }
                scan -= 1;
            }
        }
        // 属性と、`unsafe` ブロックを右辺に取る行の左側だけ読み飛ばす。
        if trimmed.starts_with("#[") || trimmed.starts_with("#!") || trimmed.ends_with('=') {
            continue;
        }
        return false;
    }
    false
}

/// コミットメッセージの文体規約を適用し始めた時点。
///
/// これより前のコミットは規約を定める前に書かれたもので、違反ではない。
///
/// # なぜハッシュではなく日付なのか
///
/// **ハッシュを埋め込むと履歴の書き換えで壊れる。** 書き換えると全コミットの
/// ハッシュが変わり、埋め込んだ値は存在しないコミットを指す。そうなると
/// `git log` が失敗して検査自体が動かなくなり、`git bisect` の途中でも
/// 同じことが起きる（bisect は実際に使っている手法である。
/// `docs/troubleshooting.md` の alt-offset の項を参照）。
///
/// 日付なら、書き換えでコミッタ日時が保たれる限り壊れない。タグを使う案も
/// あるが、タグは書き換え後に貼り直しが要る。
///
/// 値の根拠は、規約に沿って書いた最初のコミットが 2026-07-22 08:48:45、
/// その 1 つ前が 08:07:52 で、その間にコミットが無いことである。
/// 境界のちょうど中間を採ってある。どちらのコミットの秒とも一致させて
/// いないので、書き換えで秒がずれても境界は動かない。
///
/// # タイムゾーンを明示する
///
/// オフセットを省くと、git は実行環境のローカルタイムゾーンとして解釈する。
/// **境界の前後の余裕は前後 20 分程度しかないので、TZ が 1 時間ずれれば
/// 境界を飛び越える。** 別のマシンで動かしたときや TZ 設定を変えたときに、
/// 検査対象が静かに変わることになる。ISO 8601 でオフセットまで書く。
const COMMIT_STYLE_SINCE: &str = "2026-07-22T08:30:00+09:00";

/// Markdown ドキュメントの機械的に判定できる文体規則を見る（`docs/coding-standards.md` §1）。
///
/// # なぜ xtask に持つのか。ローカルの補助スクリプトでは規律のままである
///
/// 文体の検査にはローカル専用の補助スクリプトがあるが、**それは追跡対象外なので
/// `cargo xtask check` から呼べない**（呼ぶと、公開されるリポジトリの中から
/// 存在しないファイルを指すことになる）。回し忘れが実際に起き、閉じ括弧の直後に
/// 半角空白が入った差分がコミットされた。**規律で守っている箇所が残っていた**ので、
/// 機械で判定できる 2 つだけをここへ移し、コミット前に構造的に止まるようにする。
///
/// # ここで見るのは 2 つだけである。**補助スクリプトの代わりにはならない**
///
/// - S1: GFM タスクリストのマーカー直後に半角空白があること
/// - S2: 行結合の痕跡（和文の句読点・閉じ括弧の直後の半角空白、および直前に
///   空白の無い `/` の直後の半角空白）
///
/// **レンダリング結果を要する検査は移していない**（閉じない強調、タグ列の比較、
/// 見出しレベルの飛び、リンク先の生存）。Markdown レンダラが必要で、そのために
/// 新しいクレート依存を増やす判断はしていない。**したがって補助スクリプトは
/// 引き続き要る。** ここが覆うのは「回し忘れても落ちる」範囲だけである。
///
/// # 対象は追跡対象の `.md` だけ
///
/// `git ls-files` で引く。未追跡の作業メモを対象にすると、コミットに関係のない
/// ファイルで落ちる。
/// バックティックの中のパスが実在しないことを承知で書いている箇所（TAB-1 の後の精査）。
///
/// # なぜ許可リストが要るのか
///
/// **「消したファイルについての記録」は、実在しないパスを書くのが正しい。**
/// **文言では分けられない**ので、箇所を列挙する。
///
/// **`.claude` の索引の検査と同じ形である**——**列挙で守る検査なので、
/// 新しく足したら一覧へも足すこと。**
///
/// # 足してよいのは、実在しないことに意味がある場合だけである
///
/// **「直す」と「ここへ足す」の分かれ目が要る**——**足すほうが常に楽なので、
/// 基準が無いと落ちるたびに 1 件ずつ増える**（運用者の指摘。2026-09-06）。
///
/// **足してよい形は 2 つだけである。**
///
/// - **消したこと自体の記録**（「このファイルを消したら検査が落ちた」）
/// - **当時の在り処の記録**（参照実装・改名の前の名前。**記録は書き換えない**ので、
///   本文は当時のまま残り、Addendum が現在の在り処を指す）
///
/// **「意図して追跡していないファイル」は、この一覧ではなく
/// [`UNTRACKED_BY_DESIGN`] へ置く**——**ファイルの性質であって、
/// それを書いた文書の性質ではないからである。**
///
/// **それ以外は直す側である。** **パスが古くなっただけなら、書き換えればよい。**
///
/// **1 件につき理由を 1 行添えること。** **理由の書けない項目は、直す側である。**
const DOC_PATH_ALLOWLIST: &[(&str, &str)] = &[
    // **消したこと自体の記録である**（`unsafe/SAFETY` の検査が消えたファイルを
    // 読もうとして落ちた話）。
    ("docs/troubleshooting.md", "kernel/src/console/grid.rs"),
    // **当時の参照実装である。** **M2-0a で `common` へ移った**と本文が書いている。
    (
        "docs/adr/0004-panic-policy-halt-and-dump.md",
        "kernel/src/cpu.rs",
    ),
    // **当時の在り処である。** **`probes/` へ改名した**（2026-09-06）。
    // **ADR の本文は書き換えず、Addendum が現在の在り処を指している。**
    (
        "docs/adr/0015-uncacheable-mmio-mapping.md",
        ".local-probes/m3c-flush-cost-probe.patch",
    ),
];

/// **意図して追跡していないファイル**（`ADR-0031`。**個人の環境設定を公開
/// リポジトリの設定に混ぜない**）。
///
/// # なぜ (ファイル, パス) の組ではなく、パスだけで持つのか
///
/// **追跡外であることは、その**ファイル**の性質であって、それを書いた文書の
/// 性質ではない。** **組で持つと、同じ名前に触れる文書が増えるたびに 1 行
/// 増える**——**実際に増えた**（実測。2026-09-06。**`ADR-0031` と
/// `verification-coverage.md` で足りると思ったら、`troubleshooting.md` の
/// 記録で 3 箇所目が出た**）。
///
/// **上の [`DOC_PATH_ALLOWLIST`] とは意味が違う。** **あちらは「その文書の、
/// その 1 箇所」を許すもので、こちらは「このパスはリポジトリに無いのが正しい」
/// である。**
const UNTRACKED_BY_DESIGN: &[&str] = &[".claude/settings.local.json"];

/// バックティックの中のパスを、この順で前置して探す（TAB-1 の後の精査）。
///
/// **文書は `kernel/src/` を省いた断片で書くことが多い**（実測で 12 件）
/// ——`heap/allocator.rs` のような形である。**除外せず、前置して確かめる。**
const DOC_PATH_PREFIXES: &[&str] = &[
    "",
    "kernel/src/",
    "kernel/",
    "common/src/",
    "common/",
    "xtask/src/",
    "bootloader/src/",
];

/// 追跡下の `.md` から、リンク先・アンカー・バックティックの中のパスの生存を見る。
///
/// # `tools/docstyle.py` から移した（2026-09-06）
///
/// **`S5`（リンクとアンカーの生存）は「レンダリング結果を見る必要がある」を理由に
/// 移していなかったが、実装を読むと正規表現と `git ls-files` だけで、
/// レンダラを使っていない**（実測。`docs/verification-coverage.md` の
/// 「文体の検査を補助スクリプトからxtaskへ移した範囲」の表を直した）。
///
/// # バックティックの中のパスも見る
///
/// **リンクではない参照が、25 日間リポジトリに無いファイルを指していた**
/// （`probes/` の調査。2026-09-06。**当時の名前は `.local-probes/` である**）。**`docstyle` の `S5` はリンクしか
/// 見ないので、その形は素通りする。**
/// 自前の libc の純粋な関数を、ホストで建てて走らせる（C-c。`ADR-0057` の Decision 5）。
///
/// # QEMU を起こさない
///
/// **`ADR-0045` と同じ形である**——**ハード依存の無いロジックは、ホストで固定する。**
/// **`kernel/userland/libc_string.c` はシステムコールを 1 つも出さない。**
///
/// # 同じ源を 2 度建てる
///
/// **ZaytOS 向け（freestanding）と、ホスト（この項目）である。**
/// **名前が `zt_` で始まるのは、ホストの libc と衝突させないためである**
/// （`libc_string.c` の doc）。
///
/// # `cc` が無ければ落ちる
///
/// **黙って飛ばさない。** **飛ばすと「検査が在るのに走っていない」形になる**
/// ——**この体制がいちばん嫌う形である。** **前提は `README` に在る。**
fn check_libc_string_tests(workspace_root: &Path) -> Result<String> {
    let userland = workspace_root.join("kernel/userland");
    let binary = workspace_root.join("target/libc-string-test");
    let compile = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()))
        .current_dir(workspace_root)
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&binary)
        .arg(userland.join("libc_string.c"))
        .arg(userland.join("libc_string_test.c"))
        .output()
        .context("failed to run cc for the libc string tests")?;
    if !compile.status.success() {
        bail!(
            "cc failed for the libc string tests:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
    }
    let run = Command::new(&binary)
        .output()
        .context("failed to run the libc string tests")?;
    let stdout = String::from_utf8_lossy(&run.stdout).trim().to_string();
    if !run.status.success() {
        bail!("the libc string tests failed:\n{stdout}");
    }
    Ok(stdout)
}

fn check_markdown_references(workspace_root: &Path) -> Result<Vec<String>> {
    let tracked = tracked_paths(workspace_root, &["*"])?;
    let markdown = tracked_paths(workspace_root, &["*.md"])?;

    // 見出しからアンカーを作る（`docstyle` の `S5` と同じ作り方）。
    let mut anchors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut bodies: BTreeMap<String, String> = BTreeMap::new();
    for rel in &markdown {
        let Ok(text) = fs::read_to_string(workspace_root.join(rel)) else {
            continue;
        };
        let mut got = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim_start_matches('#');
            if trimmed.len() < line.len() && trimmed.starts_with(' ') {
                got.push(heading_slug(trimmed.trim()));
            }
        }
        anchors.insert(rel.clone(), got);
        bodies.insert(rel.clone(), text);
    }

    let mut findings = Vec::new();
    for rel in &markdown {
        let Some(text) = bodies.get(rel) else {
            continue;
        };
        let base = rel.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            for target in markdown_link_targets(line) {
                if target.starts_with("http://")
                    || target.starts_with("https://")
                    || target.starts_with("mailto:")
                {
                    continue;
                }
                let (path, fragment) = match target.split_once('#') {
                    Some((path, fragment)) => (path, Some(fragment)),
                    None => (target.as_str(), None),
                };
                let full = if path.is_empty() {
                    rel.clone()
                } else if base.is_empty() {
                    path.to_string()
                } else {
                    format!("{base}/{path}")
                };
                let normal = full.trim_end_matches('/').to_string();
                if !path.is_empty()
                    && !tracked
                        .iter()
                        .any(|t| *t == normal || t.starts_with(&format!("{normal}/")))
                {
                    findings.push(format!("{rel}:{number}: リンク先が存在しない -> {target}"));
                    continue;
                }
                if let Some(fragment) = fragment {
                    if let Some(got) = anchors.get(&normal) {
                        if !got.iter().any(|slug| slug == fragment) {
                            findings
                                .push(format!("{rel}:{number}: アンカーが存在しない -> {target}"));
                        }
                    }
                }
            }
            for path in backticked_paths(line) {
                if DOC_PATH_ALLOWLIST
                    .iter()
                    .any(|(file, allowed)| *file == rel && *allowed == path)
                    || UNTRACKED_BY_DESIGN.contains(&path.as_str())
                {
                    continue;
                }
                // **追跡下だけを見る。** **作業ツリーに在るかは見ない**——
                // **見ると、手元では通って CI では落ちる**（実測。2026-09-06。
                // **`.claude/settings.local.json` は`ADR-0031`で追跡外と決めてあり、
                // 手元にだけ在る**）。**この検査が主張したいのは
                // 「リポジトリが、リポジトリに無いものを指していないこと」である。**
                let found = DOC_PATH_PREFIXES
                    .iter()
                    .any(|prefix| tracked.contains(&format!("{prefix}{path}")));
                if !found {
                    findings.push(format!(
                        "{rel}:{number}: バックティックの中のパスが存在しない -> `{path}`"
                    ));
                }
            }
        }
    }
    Ok(findings)
}

/// 見出しの文字列からアンカーの綴りを作る（`docstyle` の `S5` と同じ規則）。
fn heading_slug(heading: &str) -> String {
    let mut slug = String::new();
    for ch in heading.to_lowercase().chars() {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            slug.push(ch);
        } else if ch == ' ' {
            slug.push('-');
        }
    }
    slug
}

/// `[...](...)` の行き先を集める。
fn markdown_link_targets(line: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes: Vec<char> = line.chars().collect();
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] == '[' {
            if let Some(close) = (at + 1..bytes.len()).find(|i| bytes[*i] == ']') {
                if close + 1 < bytes.len() && bytes[close + 1] == '(' {
                    if let Some(end) = (close + 2..bytes.len()).find(|i| bytes[*i] == ')') {
                        targets.push(bytes[close + 2..end].iter().collect());
                        at = end + 1;
                        continue;
                    }
                }
            }
        }
        at += 1;
    }
    targets
}

/// バックティックで囲まれた、`/` を含むパス様の文字列を集める。
///
/// **拡張子を持つものだけを見る。** **コマンドの断片（`mke2fs -d` など）を
/// パスと取り違えないためである**——**偽陽性を規則で外し、許可リストを短く保つ。**
/// **`target/` の下は生成物なので見ない。**
fn backticked_paths(line: &str) -> Vec<String> {
    const EXTENSIONS: &[&str] = &[
        ".rs", ".md", ".py", ".toml", ".json", ".ld", ".patch", ".txt", ".img", ".efi", ".elf",
    ];
    let mut paths = Vec::new();
    for piece in line.split('`').skip(1).step_by(2) {
        if !piece.contains('/') || piece.contains(' ') || piece.starts_with("target/") {
            continue;
        }
        if !EXTENSIONS.iter().any(|ext| piece.ends_with(ext)) {
            continue;
        }
        if piece.contains("..") {
            // **`#[path]` に書く相対パスの引用である**（`../../common/src/text.rs`）。
            continue;
        }
        // **形で外す**（実測で 3 件出た。2026-09-06）。
        //
        // **`*` は寄せ書き**（`kernel/userland/*.rs`）、**`<` と `>` は差し込みの
        // 場所**（`common/src/<name>.rs`）、**`=` と `:` はコマンドの引数**
        // （`cargo:rustc-link-arg=-T{manifest_dir}/link.ld`）**である。**
        // **どれも「1 つのファイルを指す参照」ではないので、生存を問えない。**
        //
        // **許可リストではなく規則で外す**——**許可リストは「実在しないと
        // 承知で書いた箇所」のためのもので、短く保つ。**
        if piece.contains(['*', '<', '>', '{', '}', '=', ':']) {
            continue;
        }
        paths.push(piece.to_string());
    }
    paths
}

/// 追跡下の `.md` の構造を見る（フェンスの対と、見出しレベルの飛び）。
///
/// # `tools/docstyle.py` から移した（2026-09-06）
///
/// **`S4` も「レンダリング結果を見る必要がある」を理由に移していなかったが、
/// 実装は行を数えるだけでレンダラを使っていない**（実測）。
fn check_markdown_structure(workspace_root: &Path) -> Result<Vec<String>> {
    let markdown = tracked_paths(workspace_root, &["*.md"])?;
    let mut findings = Vec::new();
    for rel in &markdown {
        let Ok(text) = fs::read_to_string(workspace_root.join(rel)) else {
            continue;
        };
        let mut fences = 0usize;
        let mut in_fence = false;
        let mut previous = 0usize;
        for (index, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("```") {
                fences += 1;
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
            let level = line.len() - line.trim_start_matches('#').len();
            if level == 0 || !line[level..].starts_with(' ') {
                continue;
            }
            if previous > 0 && level > previous + 1 {
                findings.push(format!(
                    "{}:{}: 見出しレベルが {} から {} へ飛んでいる",
                    rel,
                    index + 1,
                    previous,
                    level
                ));
            }
            previous = level;
        }
        if !fences.is_multiple_of(2) {
            findings.push(format!(
                "{rel}: コードフェンスが閉じていない（{fences} 本）"
            ));
        }
    }
    Ok(findings)
}

/// `git ls-files` の結果を集める。
fn tracked_paths(workspace_root: &Path, patterns: &[&str]) -> Result<Vec<String>> {
    let mut args = vec!["ls-files"];
    args.extend_from_slice(patterns);
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(&args)
        .output()
        .context("failed to run git ls-files")?;
    if !output.status.success() {
        bail!("git ls-files failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn check_markdown_prose_style(workspace_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["ls-files", "*.md"])
        .output()
        .context("failed to run git ls-files for the markdown style check")?;
    if !output.status.success() {
        bail!("git ls-files failed for the markdown style check");
    }
    let listing = String::from_utf8_lossy(&output.stdout);

    let mut findings = Vec::new();
    for rel in listing.lines().filter(|l| !l.trim().is_empty()) {
        let path = workspace_root.join(rel);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let mut in_fence = false;
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;

            // S1 はコードフェンスの内外を問わず全行を見る（補助スクリプトと同じ）。
            if let Some(rest) = task_list_marker_rest(line) {
                if !rest.starts_with(' ') && !rest.is_empty() {
                    findings.push(format!(
                        "{rel}:{number}: S1 タスクリストのマーカー直後に空白が無い: {}",
                        line.trim()
                    ));
                }
            }

            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            // フェンス内・インデントコード・表の行は S2 の対象にしない。
            if in_fence || line.starts_with("    ") || line.trim_start().starts_with('|') {
                continue;
            }
            if let Some(column) = join_trace_column(line) {
                findings.push(format!(
                    "{rel}:{number}: S2 行結合の痕跡（列 {column}）: {}",
                    line.trim()
                ));
            }
        }
    }
    Ok(findings)
}

/// GFM タスクリストのマーカー（`- [ ]` / `- [x]`）に一致したら、その直後を返す。
fn task_list_marker_rest(line: &str) -> Option<&str> {
    let rest = line.trim_start();
    let rest = rest.strip_prefix(['-', '*', '+'])?;
    let rest = rest.strip_prefix(' ')?;
    let rest = rest.strip_prefix('[')?;
    let mut chars = rest.chars();
    let marker = chars.next()?;
    if !matches!(marker, ' ' | 'x' | 'X') {
        return None;
    }
    chars.as_str().strip_prefix(']')
}

/// 行結合の痕跡を探し、見つかった位置（1 起点の文字数）を返す。
///
/// 探すのは 2 形である。**コードスパン（バッククォート）の中は対象にしない。**
///
/// - 和文の句読点・閉じ括弧（`、。」』）`）の直後が半角空白 1 個 + 非空白
/// - `/` の直後が半角空白 1 個 + 非空白で、かつ `/` の直前が空白でも `/` でもない
///
/// 後者の条件は、箇条書きの区切りに使う ` / ` を許し、行を結合したときに現れる
/// `語/ 語` の形だけを捕まえるためである。
fn join_trace_column(line: &str) -> Option<usize> {
    let masked = mask_code_spans(line);
    for (index, window) in masked.windows(3).enumerate() {
        let [current, next, after] = [window[0], window[1], window[2]];
        if next != ' ' || after.is_whitespace() {
            continue;
        }
        if matches!(current, '、' | '。' | '」' | '』' | '）') {
            return Some(index + 1);
        }
        if current == '/' {
            let previous = index.checked_sub(1).map(|i| masked[i]);
            let guarded = matches!(previous, Some(' ') | Some('/'));
            if !guarded {
                return Some(index + 1);
            }
        }
    }
    None
}

/// バッククォートで囲まれた範囲を、判定に引っかからない文字で潰す。
///
/// 潰す先は `\0` にしてある。**空白にはしない。** 空白にすると、コードスパンの
/// 直後にある区切りが「直前が空白」に見えて、S2 の条件が変わってしまう。
fn mask_code_spans(line: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    let mut in_span = false;
    for ch in line.chars() {
        if ch == '`' {
            in_span = !in_span;
            out.push('\0');
            continue;
        }
        out.push(if in_span { '\0' } else { ch });
    }
    out
}

/// 本文の行数の規則を書いたコミット（`CLAUDE.md` の「コミットメッセージ」）。
///
/// # なぜハッシュで書くのか
///
/// **規則ごとに当てる範囲が違うからである。** **接頭辞と 2 行目の空行は
/// 履歴全体が既に満たしているが**（実測）、**本文の行数（2 行から 5 行）は
/// 遡って当てると落ちる**——**運用を変える前は 20 行を超える本文が普通だった**
/// （実測で最長 77 行）。**したがって、この地点より後にだけ当てる。**
///
/// # この値は動かない
///
/// **`CLAUDE.md` の「コミットメッセージ」が、公開後は履歴を書き換えられないと
/// 書いている**ので、
/// **ハッシュは後から変わらない。** 日付で書く手もあるが、
/// **「規則を書いたのはどれか」を指すほうが、なぜその地点なのかが読める。**
const COMMIT_BODY_RULE_COMMIT: &str = "69d2a9e";

/// 件名の接頭辞として許すもの（`CLAUDE.md` の「コミットメッセージ」）。
///
/// **履歴の 576 件すべてがこの 7 つのいずれかである**（実測）。
/// **したがって履歴全体へ当てられる。**
const COMMIT_SUBJECT_PREFIXES: &[&str] =
    &["feat", "fix", "docs", "refactor", "test", "style", "chore"];

/// 1 つのコミットメッセージを見る。**純粋関数である。**
///
/// # なぜ切り出すのか
///
/// **`git` を動かさずに試験できるようにするためである。** **実際のコミットを
/// 作って確かめる形は後始末に `git reset --hard` が要り、未コミットの変更を
/// 巻き込む**——**この段で実際に踏んで、書きかけの実装を消した。**
///
/// # 範囲の判定は呼び出し側が持つ
///
/// **規則ごとに当てる範囲が違う**ので、**ここへは「当てるかどうか」だけを渡す。**
fn commit_message_findings(
    short: &str,
    message: &str,
    body_rule: bool,
    gap_rule: bool,
) -> Vec<String> {
    let lines: Vec<&str> = message.trim_end_matches('\n').split('\n').collect();
    let subject = lines.first().copied().unwrap_or("");
    let mut findings = Vec::new();

    let prefix = subject.split_once(':').map(|(p, _)| p);
    if !prefix.is_some_and(|p| COMMIT_SUBJECT_PREFIXES.contains(&p)) {
        findings.push(format!(
            "{short}: the subject prefix is not one of {COMMIT_SUBJECT_PREFIXES:?} \
             (CLAUDE.md の「コミットメッセージ」): {subject}"
        ));
    }

    if lines.len() > 1 && !lines[1].trim().is_empty() {
        findings.push(format!(
            "{short}: the second line must be blank when there is a body \
             (CLAUDE.md の「コミットメッセージ」): {subject}"
        ));
    }

    if body_rule {
        let body = lines
            .iter()
            .skip(2)
            .filter(|line| !line.trim().is_empty())
            .count();
        if body != 0 && !COMMIT_BODY_LINES.contains(&body) {
            findings.push(format!(
                "{short}: the body is {body} line(s); it must be {} to {} \
                 (CLAUDE.md の「コミットメッセージ」): {subject}",
                COMMIT_BODY_LINES.start(),
                COMMIT_BODY_LINES.end()
            ));
        }
    }

    if gap_rule && japanese_ascii_gap(subject) {
        findings.push(format!(
            "{short}: the subject has a space between Japanese and ASCII \
             (docs/coding-standards.md): {subject}"
        ));
    }

    findings
}

/// コミットメッセージの、機械で判定できる規則を見る（`CLAUDE.md` の「コミットメッセージ」）。
///
/// # 規則ごとに当てる範囲が違う
///
/// **履歴全体**——接頭辞と、本文があるときの 2 行目の空行。**どちらも履歴が
/// 既に満たしている**（実測）。
/// **[`COMMIT_BODY_RULE_COMMIT`] より後**——本文の行数。
/// **[`COMMIT_STYLE_SINCE`] より後**——和文と英数字の間の空白（元からある規則）。
///
/// # 判定できないもの
///
/// **「件名だけで足りるなら 1 行で終える」は機械では見られない。**
/// **「足りる」かどうかは中身の判断だからである。**
/// **1 行のコミットも本文つきのコミットも、どちらも通す。**
fn check_commit_message_style(workspace_root: &Path) -> Result<Vec<String>> {
    let all = read_commit_messages(workspace_root, &["HEAD"])?;
    let hashes = |range: &[&str]| -> Result<std::collections::BTreeSet<String>> {
        Ok(read_commit_messages(workspace_root, range)?
            .into_iter()
            .map(|(hash, _)| hash)
            .collect())
    };
    let after_rule = hashes(&[&format!("{COMMIT_BODY_RULE_COMMIT}..HEAD")])?;
    let after_cutoff = hashes(&[&format!("--since={COMMIT_STYLE_SINCE}"), "HEAD"])?;

    let mut findings = Vec::new();
    for (hash, message) in &all {
        findings.extend(commit_message_findings(
            &hash[..7.min(hash.len())],
            message,
            after_rule.contains(hash),
            after_cutoff.contains(hash),
        ));
    }
    Ok(findings)
}

/// `git log` からハッシュと本文の対を読む。
fn read_commit_messages(workspace_root: &Path, range: &[&str]) -> Result<Vec<(String, String)>> {
    let mut args: Vec<&str> = std::vec!["log", "--format=%H%x1f%B%x1e"];
    args.extend_from_slice(range);
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(&args)
        .output()
        .context("failed to read commit messages")?;
    if !output.status.success() {
        bail!("git log failed while reading commit messages");
    }
    let text = String::from_utf8(output.stdout).context("git log produced non-UTF-8")?;
    let mut out = Vec::new();
    for record in text.split('\u{1e}') {
        let record = record.trim_start_matches('\n');
        if let Some((hash, message)) = record.split_once('\u{1f}') {
            out.push((hash.to_string(), message.to_string()));
        }
    }
    Ok(out)
}

fn japanese_ascii_gap(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for window in chars.windows(3) {
        if window[1] != ' ' {
            continue;
        }
        let (left, right) = (window[0], window[2]);
        if (is_japanese(left) && is_ascii_word_start(right))
            || (is_ascii_word_end(left) && is_japanese(right))
        {
            return true;
        }
    }
    false
}

fn is_japanese(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}')
}

fn is_ascii_word_start(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '(' || c == '（'
}

fn is_ascii_word_end(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == ')' || c == '）'
}

/// ACPI の検証経路の破壊確認（S1-b-2）。
///
/// **観測は panic ではない。** S1 の ACPI 経路は異常を見つけても停止しないので、
/// 「検出のログが出ること」と「MADT の列挙が完了しないこと」の 2 つで判定する。
/// さらにハートビートを期待マーカーに入れて、**検出した後もカーネルが動き続ける
/// こと**まで見る。停止してしまえば機能的な後退であり、それはそれで異常である。
///
/// 禁止マーカーの `acpi: MADT enumeration complete` は、**列挙が最後まで通った
/// ときにだけ出る行**である。検出をすり抜けた場合にこれが出る。
const ACPI_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "bad-signature",
        feature: "acpi-test-bad-signature",
        expected_markers: &[
            "acpi: the MADT at",
            "has the wrong signature",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["acpi: MADT enumeration complete"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "bad-checksum",
        feature: "acpi-test-bad-checksum",
        expected_markers: &["failed its checksum", "heartbeat: ticks="],
        forbidden_markers: &["acpi: MADT enumeration complete"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "bad-length",
        feature: "acpi-test-bad-length",
        // 署名の破れ（`the wrong signature`）とは別の経路であることを、
        // 期待する文言そのもので示す。
        expected_markers: &["failed validation: LengthTooSmall", "heartbeat: ticks="],
        forbidden_markers: &["acpi: MADT enumeration complete"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "zero-entry-length",
        feature: "acpi-test-zero-entry-length",
        // **チェックサムではなくエントリ長で止まったことを確かめる。** 破壊側で
        // チェックサムを合わせ直してあるので、`failed its checksum` は出ない。
        expected_markers: &[
            "acpi: the MADT entry walk stopped",
            "ZeroLength",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["acpi: MADT enumeration complete", "failed its checksum"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "unmapped-rsdp",
        feature: "acpi-test-unmapped-rsdp",
        expected_markers: &[
            "[acpi-test-unmapped-rsdp] redirecting",
            "which the live page table does not map",
            "heartbeat: ticks=",
        ],
        // 差し替え先が見つからなければ破壊が成立しない。**「壊したつもり」で
        // 緑になる形を塞ぐ。**
        forbidden_markers: &[
            "acpi: MADT enumeration complete",
            "the sabotage did nothing",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "rsdp-outside-window",
        feature: "acpi-test-rsdp-outside-window",
        expected_markers: &[
            "[acpi-test-rsdp-outside-window] redirecting",
            "outside the direct map window",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &[
            "acpi: MADT enumeration complete",
            "the sabotage did nothing",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// 本文の行数の下限と上限（`CLAUDE.md` の「コミットメッセージ」）。**空行は数えない。**
const COMMIT_BODY_LINES: core::ops::RangeInclusive<usize> = 2..=5;

/// `-smp 2` での ACPI 列挙の確認（S1-b-2）。
///
/// **既定ビルドである**（`feature` が空）。壊すのではなく、コア数を変えた構成で
/// 列挙結果が QEMU の指定と一致することを見る。roadmap の S1 到達条件に
/// 「`-smp 2` で起動しても列挙結果が QEMU の指定と一致すること」とあるのに、
/// **手動実行でしか確かめていなかった。手動確認は再現されず、必ず腐る。**
///
/// 全項目を複数のコア数で回すと項目数も所要時間もそのまま倍になるので、
/// **この 1 項目だけを 2 コアで回す。** 費用は QEMU 起動 1 回である。
/// S3-b-2a の tripwire の破壊確認（`--percpu-test <kind>`）。
///
/// `task::require_bootstrap_processor` は GPR 照合デモが bootstrap processor 以外で
/// 走ることを拒む。`cpu_id()` に非 `0` を返させて、**その分岐が働くこと**を見る。
///
/// # **示すのは分岐が働くことだけである**
///
/// **実際の並行アクセスは示さない。** `GPR_BUF` が別コアから同時に触られる状況を
/// 作ってはおらず、AP は起こしていない。**機序の直接観測は「AP がタスクを実行する
/// 段」の到達条件である**（`roadmap.md`）。
///
/// # この破壊は `MAX_CPUS > 1` でなければ作れない
///
/// `MAX_CPUS = 1` のまま非 `0` を返すと `this_cpu_ptr` が配列外を指し、
/// **破壊が別の未定義動作を作ってしまう。** S3-b-2a で `MAX_CPUS` を 2 へ
/// 上げたので初めて構成できるようになった。**tripwire の破壊確認には、
/// tripwire が守ろうとしている能力そのものが必要である。**
/// 設置した AP トランポリンが雛形と一致することの破壊確認（S3-b-2b-1）。
///
/// **壊すのはコピーであって雛形ではない。** 雛形を壊すとコピー元が変わるだけで
/// 両方が同じ値になり、比較は通ってしまう。**検査が見ているのは「コピーとパッチが
/// 正しく行われたか」なので、壊すべきはコピー側である。**
/// AP の per-CPU 資産と CURRENT の sentinel の破壊確認（S3-b-2b-2）。
const SMP_AP_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "ap-touch-scheduler",
        feature: "smp-ap-touch-scheduler-test",
        expected_markers: &[
            "is about to read the scheduler",
            "CURRENT is still the sentinel",
            "halting",
        ],
        // **BSP は走り続ける**（止まるのは AP だけ）ので、ハートビートは出る。
        // 禁止するのは **AP が最後まで進んだこと**である。丸めていたら停止せず、
        // タスク 0 を走らせているように見えたまま、ここまで来たはずである。
        forbidden_markers: &["is up with its own per-CPU state"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "ap-timer",
        feature: "",
        expected_markers: &[
            // AP が自分の SVR を書き、**ソフトウェア有効化が立つこと**。
            "ap 1 wrote its own SVR",
            "software_enabled=true",
            // AP が自分の LVT タイマを開けたこと。
            "ap 1 armed its own LAPIC timer",
            // **コアごとのハートビート。** AP 側は BSP と別の行である。
            "smp: ap heartbeat: cpu=1",
            // **AP のティックが進んでいること。** `cpu1=0` を禁止マーカーで落とす
            // だけだと、行そのものが出ない構成を通してしまう。
            "ap_ticks=cpu1=",
            // **会計が閉じること。**
            "timer_accounting_balanced=true",
            // 定常状態まで到達すること。
            "heartbeat: ticks=",
        ],
        // **AP のティックが 1 本も進んでいない状態を落とす。** ハートビートは
        // 100 ティックごとなので、1 本目のハートビートが出る時点で AP は既に
        // 数え始めている。
        forbidden_markers: &[
            "ap_ticks=cpu1=0,",
            "timer_accounting_balanced=false",
            "halting",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "ap-no-svr",
        feature: "smp-ap-timer-no-svr-test",
        expected_markers: &[
            "is skipping its own SVR write (sabotage)",
            // **落ちるのは「開けられない」ではなく「届かない」である。**
            // LVT への書き込みそのものは成功する（実測）。SVR の bit 8 が
            // 落ちているので Local APIC が無効で、**割り込みが 1 本も配送されない。**
            // したがって AP のティックが 0 のまま止まる。
            //
            // **BSP の書き込みが AP に効いていないことの実証でもある。**
            // 効いていれば、AP が書かなくてもティックが来てしまう。
            "ap_ticks=cpu1=0,",
            // BSP は走り続ける。**起動が死んだのではないことを分けて見る。**
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["smp: ap heartbeat: cpu=1"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "ap-share-ticks",
        feature: "smp-ap-timer-share-ticks-test",
        expected_markers: &[
            // **会計が閉じないことを名指しで見る。** 合計が配送数のおよそ 2 倍になる。
            "timer_accounting_balanced=false",
        ],
        forbidden_markers: &["timer_accounting_balanced=true"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **S4-a の `ap-enter-scheduler` はここにあった。S4-c-3-2b で引退した。**
    // 破壊の対象だった「AP を手前で返す分岐」が本番から消えたので、
    // **「分岐を外す」破壊は構成できない。** 役目（sentinel が止めることの実証）は
    // 下の `ap-no-sentinel-clear` が引き継いでいる。**引退の記録は
    // `docs/verification-coverage.md` の破壊 feature 一覧にある。**
    CriticalTest {
        name: "ap-no-sentinel-clear",
        feature: "smp-ap-no-sentinel-clear",
        expected_markers: &[
            // **sentinel が止める。** `ap-enter-scheduler` から引き継いだ主張で、
            // **同じ行**を見ている（引き継ぎが成立していることの担保）。
            "CURRENT is still the sentinel",
        ],
        // AP は最初のティックで止まるので、AP のハートビートは出ない。
        forbidden_markers: &["smp: ap heartbeat: cpu=1"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **tripwire の機序の直接観測（S4-c-4-1）。**
    //
    // 主マーカーは**停止行そのもの**である。AP のハートビートが出ないことは
    // **補助**であって、主マーカーにはしない（回数より機序の直接観測）。
    CriticalTest {
        name: "ap-runs-preemptive-demo",
        feature: "smp-ap-runs-preemptive-demo",
        expected_markers: &["may only run on the bootstrap processor"],
        forbidden_markers: &[
            // tripwire が鳴らずに戻ってきた形。
            "the tripwire did not fire",
            // 補助: 入口で止まるので AP はタイマを開けず、ハートビートも出ない。
            "smp: ap heartbeat: cpu=1",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **S4-c-4-2 の梯子。ここに置けたのは下 2 段だけである。**
    //
    //   tripwire 外し                 → AP がワーカーを取れない = 第 1 層の実証（対照）
    //   tripwire 外し + ignore-owner  → AP がワーカーを取れる   = 第 1 層の実証（本命）
    //
    // **この 2 つは対になって第 1 層の効きを直接示す。** 差は第 1 層 1 つだけで、
    // 「取れない」と「取れる」が `selected task ... owned by cpu` の 1 行で分かれる。
    //
    // **第 2 層と検出器の実証はここには置けなかった。** デモ経由では窓が閉じる——
    // `setup_preemptive_tasks` が `demo_deadline` を**呼んだコアのティック**で
    // 決めるのに対し、`on_timer_tick` の締切判定は**各コアが自分のティック**で
    // 行う。AP のティックは 0、bootstrap processor は既に数百なので、
    // **BSP は次のティックでワーカーを `Blocked` に戻す。** 競合が起きないまま
    // 窓が閉じるので、「検出行が出ない」は守りの効きを示さない。
    // 詳細と代案は `docs/verification-coverage.md` にある。
    //
    // **「窓があるか」は `selected task ... owned by cpu` の 1 行で見る。**
    // ハートビートの `ap_current` は 1 秒ごとの標本なので取り逃しうる。
    // **こちらは起きた瞬間に 1 度だけ出るので、観測が運に依存しない。**
    //
    // **判定に使わない観測量**（走らせる前に決めてある）:
    // GPR 照合の結果、`switches`、`sum(resumes)`。第 1 層を外すと `GPR_BUF` が
    // コア間で競合し、デモの会計もコア別ではないので合わない。**どちらも構成の
    // 帰結であって退行ではない。**
    // **名前は主張を指す（S4-c-4-3で改名した）。**
    //
    // 旧名は `ap-demo-layer1-holds` で「第1層が保つ」と読めたが、**第1層の実証は
    // S4-c-4-3 の梯子が担っている。** この項目が表明しているのは
    // **「`CURRENT` を外から 0 にされると、AP のアイドルタスクの `saved_rsp` が
    // 0 のまま切り替え先になり、範囲検査が止めること」**である。
    // **名前が主張を指さないと、一覧を読んだ人が誤って引退させる。**
    CriticalTest {
        name: "ap-forced-current-range-check",
        feature: "smp-ap-runs-preemptive-demo,sched-ignore-bootstrap-tripwire",
        // **AP は落ち先（自分のアイドルタスク）へ回され、そこで停止する。**
        //
        // `setup_preemptive_tasks` が `set_current_index(0)` を呼ぶので AP の
        // `CURRENT` が 0 になり、次の選択でアイドルタスクが**切り替え先**に
        // なる。その `saved_rsp` は 0 のままなので範囲検査が捕まえる。
        // **停止まで含めて表明する**——書かないと「穏やかに落ち先へ回った」と
        // 読まれるが、実際には止まっている。
        expected_markers: &[
            "starting preemptive demo",
            "is outside its stack",
            "stacks are mixed",
        ],
        forbidden_markers: &[
            // 第 1 層が効いているので、AP は担当外のワーカーを取れない。
            "the first guard layer did not keep it out",
            // **`double selection detected` はここにあった。S6-a で外した。**
            //
            // **この構成では正当に鳴りうる。** `setup_preemptive_tasks` は先頭で
            // `set_current_index(0)` を呼ぶので、**AP の `CURRENT` が一時的に
            // タスク 0 を指す。** その窓の間に bootstrap processor がティックを
            // 受けると、走行可能な担当ワーカーがまだ無いので落ち先の 0 を選び、
            // **検出器は「タスク 0 が他コアの current でもある」を見て鳴る。**
            //
            // **検出器は誤っていない。** `currents` は本当に `[0, 0]` である。
            // **誤っていたのはこの禁止マーカーのほうである**——鳴らないことを
            // 要求していたが、**鳴るかどうかは窓に重なるかどうかで決まる。**
            //
            // **この項目はもう二重選択を主張しない。** その主張は S4-c-4-3 の
            // 梯子（`smp-stimulus-*`）が担っている。詳細と、窓を広げて観測した
            // 記録は `verification-coverage.md` にある。
        ],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // **`ap-demo-layer1-off` はここにあった。S4-c-4-3 で引退した。**
    //
    // **主張が重なったためである。** あれは「第 1 層を外すと AP が担当外の
    // ワーカーを取れる」を示していたが、**同じ主張を S4-c-4-3 の梯子がより強い
    // 構成で示す**——あちらの窓は「bootstrap processor がワーカーを回している
    // 間ずっと」開くのに対し、こちらはデモ経由で**締切の食い違いに依存する
    // 短い窓**だった。**弱いほうを引退させる。**
    //
    // **上の `ap-demo-layer1-holds` は残す。** あちらは第 1 層の実証ではなく、
    // **`CURRENT` を外から 0 にされたときにアイドルタスクの `saved_rsp` が 0 の
    // まま読まれ、範囲検査が止めること**を表明している。**S4-c-4-3 の梯子は
    // その事象を作らないので、主張は重ならない。**
    // **S4-c-4-3 の梯子。隣り合う 2 つが層 1 つぶんだけ違う。**
    //
    //   刺激のみ                          → 担当外選択なし・検出行なし = 刺激は単独で何も起こさない
    //   刺激 + ignore-owner               → 担当外選択あり・検出行なし = 第 1 層と第 2 層の実証
    //   刺激 + ignore-owner + ignore-current → 検出行あり              = 検出器の実証
    //
    // **窓は 3 構成すべてに在る**（ワーカーが `Ready` で bootstrap processor が
    // 回している）。**1 段目で担当外選択が出ないのは、窓が無いからではなく
    // 第 1 層が働いているからである。** ここが S4-c-3-2b で踏んだ形との違いで、
    // **「緑だが何も検査していない」に戻らない。**
    //
    // **停止は検出行を妨げない。ただし無条件ではない。**
    // `halt_forever` が止めるのは**呼んだコアだけ**なので、片方が GPR 照合で
    // 止まってももう片方は回り続ける。**しかし検出を行うコア自身が先に停止すれば
    // マーカーは出ない。** 今回それが起きていないことは**5/5 の実測が根拠であって、
    // 機序の保証ではない。** 「観測が停止までの範囲に限られる」と同じ性質の注記である。
    //
    // **判定に使わない観測量**（走らせる前に決めてある）: GPR 照合の結果、
    // `switches`、`sum(resumes)`、デモの進捗判定。第 1 層を外すと `GPR_BUF` が
    // コア間で競合し、会計もコア別ではないので合わない。**構成の帰結であって
    // 退行ではない。**
    CriticalTest {
        name: "smp-stimulus-only",
        feature: "sched-keep-workers-runnable",
        expected_markers: &["demo workers are runnable again"],
        forbidden_markers: &[
            "the first guard layer did not keep it out",
            "double selection detected",
        ],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "smp-stimulus-layer1-off",
        feature: "sched-keep-workers-runnable,sched-ignore-owner",
        // **働いた側を直接観測する。** 「検出行が出ないこと」だけだと、
        // `GPR_BUF` の競合で系が止まった場合と区別できない（毎回止まることを
        // 実測した）。**第 2 層が候補を弾いた行を要求する。**
        expected_markers: &[
            "the first guard layer did not keep it out",
            "the second guard layer skipped a candidate",
        ],
        // **観測は停止までの範囲に限られる。** この構成では `GPR_BUF` が
        // コア間で競合し、**GPR 照合が毎回停止する**（5/5 で実測）。
        // **停止後に出るはずの行を期待マーカーへ足さないこと。**
        forbidden_markers: &["double selection detected"],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "smp-stimulus-both-layers-off",
        feature: "sched-keep-workers-runnable,sched-ignore-owner,sched-ignore-current",
        expected_markers: &[
            "the first guard layer did not keep it out",
            "double selection detected",
        ],
        forbidden_markers: &[],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **IPI が届くことの主張（S5-a）。** 破壊ではなく、**機能そのものの検査**である。
    //
    // **判定は 2 つとも要る**——「受け取った本数が送った本数と一致すること」と
    // 「AP が生き続けること」。**前者だけだと、AP が死んでいても 0 と 0 で
    // 一致してしまう。** 実際、受け口を用意する前は AP が死んで両方 0 だった。
    CriticalTest {
        name: "ipi-probe",
        feature: "smp-ipi-probe",
        expected_markers: &[
            // 送受信が一致した要約行。**本数まで含めて固定する。**
            "sent=4 received=4",
            // AP が生き続けている証拠。**ハートビートが出るのは死んでいない側だけ。**
            "smp: ap heartbeat: cpu=1",
        ],
        forbidden_markers: &[
            "did not accept a probe IPI",
            "did not handle it within the spin limit",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **世代方式の実証（S5-b）。** 世代を 1 つ上げると、**AP が次の取得で
    // フラッシュする。** 入口はティックごとに BKL を取るので、**遅くとも
    // 1 ティックで整合する。**
    //
    // **判定は 2 つとも要る**——AP がフラッシュしたことと、AP が生き続けること。
    // **前者だけだと、フラッシュして死んでいても通る。**
    CriticalTest {
        name: "tlb-generation",
        feature: "smp-tlb-generation-probe",
        // **絶対値で期待しない（S7-d で直した）。** 以前は
        // `bumped the tlb generation to 1` と `tlb_gen=1 flush_cpu1=1` を見ていたが、
        // **それは「起動の間に世代を動かすものが他に無い」という、この探りの主張とは
        // 無関係な事情に依存していた。** S7-d のアドレス空間のデモが既定ビルドで
        // 世代を 2 つ消費した時点で落ちた。
        //
        // **関係のほうを見る**（[`check_tlb_generation_relation`]）——**探りが上げた
        // 世代に AP が追いつき、そのために 1 回以上フラッシュしたこと。**
        // **カーネルの出力は変えていない。** 既にある行から関係を導いている。
        expected_markers: &["smp: ap heartbeat: cpu=1"],
        forbidden_markers: &[],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **TLB シュートダウンの実証（S5-c。S8-d で主張を作り直した）。** 世代を
    // 上げた側は AP がフラッシュし（the ap flushed）、**2 回目の触りは必ず #PF に
    // なる**——翻訳が無いので歩き、写像が無いので落ちる。フラッシュの帰結として
    // 保証される側なので、#PF のダンプ（vector=14、CR2=探りのページ）まで期待する。
    CriticalTest {
        name: "tlb-shootdown",
        feature: "smp-tlb-shootdown-probe",
        expected_markers: &[
            "the ap touched the probe page 1 time(s)",
            "the ap flushed",
            "touches 1 -> 1",
            "exception: vector=14",
            "cr2=0xffff818000000000",
        ],
        forbidden_markers: &["touches 1 -> 2", "the ap did not flush"],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // **破壊: 世代を上げない。** AP はフラッシュしない（the ap did not flush）。
    //
    // **2 回目の触りの結果は主張しない（S8-d で作り直した）。** かつては
    // 「古い翻訳で成功する（touches 1 -> 2）」を期待に置いていたが、**TLB が翻訳を
    // 保持し続けることはアーキテクチャが許しているだけで約束していない。**
    // TCG のソフトウェア TLB は無効化事象なしにエントリを捨て、どれを捨てるかは
    // バイナリのレイアウトで決まるため、無関係な変更で決定的に落ちた（S8-d）。
    // この破壊の本来の主張は「世代を上げなければ AP は世代フラッシュをしない」で
    // あり、それは flushes の不動が観測している。
    CriticalTest {
        name: "tlb-no-shootdown",
        feature: "smp-tlb-shootdown-probe,smp-tlb-no-generation-bump",
        expected_markers: &[
            "the ap touched the probe page 1 time(s)",
            "the ap did not flush",
        ],
        forbidden_markers: &["the ap flushed ("],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // **宛先の主張の破壊（S4-a）。** 確実に落ちるのは読み戻しの主張のほうで、
    // 配送が実際にどうなるかは観測していない。
    CriticalTest {
        name: "ioapic-keyboard-broadcast",
        feature: "ioapic-keyboard-broadcast-test",
        expected_markers: &["IRQ1 is not aimed at the bootstrap processor in physical mode"],
        forbidden_markers: &["heartbeat: ticks="],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// BKL 待ちのタイムアウト（S4-b-4）。**TCG で回る。**
///
/// # KVM を要さない
///
/// タイムアウトの機序は「保持者が離さない + 待ち側の TSC が進む」であって、
/// **同時実行を要さない。** 実測でも TCG のラウンドロビンで発火した
/// （起動を含めて約 15 秒）。**KVM 必須の項目を最小に保つ。**
const BKL_TIMEOUT_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "hold-forever",
    feature: "bkl-hold-forever-test",
    expected_markers: &[
        "took the lock and will never release it (sabotage)",
        // **待ちの上限に達したことと、原因の中身。**
        "has waited",
        "without acquiring the lock",
        "the lock reads as held by cpu",
        "halting",
    ],
    // **完走しない構成である。** AP が止まるので定常状態へ来ない。
    forbidden_markers: &[],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

/// BKL の破壊（S4-b-2）。**いずれも名指しの検出で停止する。**
const BKL_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "hold-with-if-set",
        feature: "bkl-hold-with-if-set-test",
        expected_markers: &[
            "enabling interrupts while holding the lock (sabotage)",
            // **再帰検出が発火する。** 保持区間 = IF=0 の不変条件が崩れると、
            // 同じコアが irq_entry から取ろうとする。
            "bkl: recursive acquisition on cpu",
            "halting",
        ],
        forbidden_markers: &[],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "hold-across-hlt",
        feature: "bkl-hold-across-hlt-test",
        expected_markers: &[
            // **保持したまま `hlt` すると、次に自分が入口へ入るときに再帰になる。**
            // 単一コアでも観測できるのはこのためである（もう一方のコアが要らない）。
            "bkl: recursive acquisition on cpu",
            "halting",
        ],
        forbidden_markers: &[],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

const SMP_TRAMP_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "corrupt-copy",
    feature: "smp-tramp-corrupt-copy-test",
    expected_markers: &["diverges from the template at offset", "halting"],
    forbidden_markers: &["application processor 1 started", "heartbeat: ticks="],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

const PERCPU_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "fake-nonzero-cpu-id",
    feature: "percpu-fake-nonzero-cpu-id",
    expected_markers: &["may only run on the bootstrap processor", "halting"],
    // 停止せずにデモへ進んでいたら、tripwire が働いていない。
    forbidden_markers: &["task: cooperative switch verified", "heartbeat: ticks="],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

/// per-CPU スロットが足りない構成の確認（`-smp 4`、S3-b-2a）。
///
/// # なぜ `-smp 4` を別に置くのか
///
/// **覆いの報告の「覆えていない」側を評価する構成を残すためである。**
/// `MAX_CPUS = 2` なので `-smp 2` では覆えてしまい、`false` の枝が通らない。
///
/// **これを置かないと、同じ失敗が再発しても次の段まで気づけない。** S3-b-1 で
/// 覆いの検査を `halt_forever` で入れたときに `-smp 2` の起動が止まり、
/// **既存の検査は緑のままだった**（見ているマーカーが停止点より前にあった）。
/// **`false` 側を評価する構成が無い状態を作らない。**
///
/// # 主張は 2 つある。**片方だけでは足りない**
///
/// - 警告が出ること（`false` 側が実際に評価されている）
/// - **完走すること**（`heartbeat: ticks=`）。**警告だけを見ると、また停止に
///   戻ったときに捕まらない。**
///
/// コア数は `cmd_marker_test` の引数なので、`-smp 2` の表とは別に持つ。
const ACPI_SMP4_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "smp4-more-cpus-than-slots",
    feature: "",
    expected_markers: &[
        "4 local APIC(s) of which 4 usable",
        // 覆いの報告の `false` 側。**MAX_CPUS を 4 以上へ上げると出なくなるので、
        // そのとき この項目が落ちる**（意図した破壊確認である）。
        "there are more usable CPUs than per-CPU slots",
        // **`MAX_CPUS` を超えるコアは起こさない**（S3-b-2b-1 の方針）。
        // 起こした分は署名を出し、**起こさなかった分があること**も主張する。
        // **後者が無いと「全部起こしてしまった」を捕まえられない。**
        "smp: application processor 1 started",
        "1 AP(s) attempted, 1 started, 2 skipped",
        // 警告を出しても停止しないこと。
        "heartbeat: ticks=",
    ],
    forbidden_markers: &["halting"],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

const ACPI_SMP_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "smp2-enumeration",
    feature: "",
    expected_markers: &[
        "acpi: MADT enumeration complete",
        "2 local APIC(s) of which 2 usable",
        // **AP が実際に起きて署名を出すこと**（S3-b-2b-1）。
        "smp: application processor 1 started",
        "1 AP(s) attempted, 1 started, 0 skipped",
        // **AP が自分の per-CPU 資産を持って本番 CR3 へ移ったこと**（S3-b-2b-2）。
        // GDTR 由来の cpu_id とデータブロックの索引が一致することも見る。
        "cpu_id() now reads 1 from GDTR",
        "match=true",
        "PML4[0] read back from this core = empty:true",
        "ap 1 is up with its own per-CPU state",
        // **`-smp 2` で定常状態まで到達すること**（S3-b-1）。
        //
        // この 1 行を足す前は、上の 2 つが出た時点で打ち切っていたので、
        // **打ち切りより後で起動が止まる退行を構造的に捕まえられなかった。**
        // 実際に踏んだ: per-CPU スロットの覆いの検査を停止付きで入れたところ、
        // 「2 コア列挙 / スロット 1」で halt して `-smp 2` の起動が止まったが、
        // **この項目は緑のままだった**（見ているマーカーが停止点より前にある）。
        //
        // 期待マーカーに入れると、打ち切りの条件が「これも出るまで待つ」に
        // 変わるので、**定常状態まで進むことが要求される。**
        "heartbeat: ticks=",
    ],
    // 1 個しか出ないのは、`-smp` が効いていないか列挙が取りこぼしているかの
    // どちらかである。**どちらも見逃したくない。**
    //
    // `halting` は**二次的な網である。** `cpu::halt_forever()` 自身はこの語を
    // 出さない（呼び出し側がログへ書いたときにだけ現れる）ので、**ログを書かずに
    // 停止する経路は捕まえられない。** 実測で確認した（停止を一時的に戻すと、
    // 落としたのは `heartbeat: ticks=` のほうで、この禁止マーカーは素通りした）。
    // **停止を捕まえている主たる根拠は、上の `heartbeat: ticks=` である。**
    forbidden_markers: &["1 local APIC(s) of which 1 usable", "halting"],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

/// `--calibration-spread` の既定の起動回数。
///
/// S1-b でメモリ型の確認に 5 回使った前例に揃えてある。
const DEFAULT_CALIBRATION_RUNS: usize = 5;

/// Local APIC タイマの較正結果を、複数回の起動にわたって集める（S2-c）。
///
/// **これは検査項目ではない。** 合否を判定せず、値を並べて出すだけである。
/// 許容幅を決めるための入力を人が読む形で集めるのが目的で、`--full` には
/// 入れていない（項目数は増えない）。
///
/// **手で 5 回回して記録する形を採らないのは、手動確認が再現されず必ず腐るから
/// である**（`-smp 2` の確認を検査項目にしたのと同じ判断）。ここでは合否を
/// 決められないので検査項目にはできないが、**手順だけはコマンドとして固定する。**
fn cmd_calibration_spread(runs: usize) -> Result<()> {
    println!("=== calibration spread over {runs} boot(s) ===");
    let mut medians: Vec<u64> = Vec::new();

    for run in 1..=runs {
        let serial = capture_serial_for_calibration(run)?;
        let mut found = false;
        for line in serial.lines() {
            if line.contains("apic: LAPIC timer calibration:") {
                println!("run {run}: {}", line.trim());
                if let Some(median) = parse_labelled_number(line, "median=") {
                    medians.push(median);
                    found = true;
                }
            } else if line.contains("calibration samples:") || line.contains("widest tick advance")
            {
                println!("run {run}: {}", line.trim());
            }
        }
        if !found {
            println!("run {run}: no calibration line was produced");
        }
    }

    if medians.is_empty() {
        return Err(anyhow::anyhow!(
            "no calibration result was captured in {runs} run(s)"
        ));
    }

    medians.sort_unstable();
    let low = medians[0];
    let high = medians[medians.len() - 1];
    let spread = high - low;
    // 相対ばらつきを ppm で出す。**許容幅はここから導く。**
    let relative_ppm = spread.saturating_mul(1_000_000) / high.max(1);
    println!(
        "--- across {} run(s) that produced a value ---",
        medians.len()
    );
    println!("min={low} Hz max={high} Hz spread={spread} Hz ({relative_ppm} ppm of max)");
    println!(
        "note: this command reports values only. it does not decide pass or fail, so it is \
         not one of the `--full` check items."
    );
    Ok(())
}

/// `label` に続く 10 進数を取り出す。
fn parse_labelled_number(line: &str, label: &str) -> Option<u64> {
    let rest = line.split(label).nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 既定ビルドを 1 回起動して serial を返す。
///
/// **feature を 1 つも足さない既定ビルドである。** 較正値は既定ビルドのものを
/// 集める（`verification-coverage.md` の「値は既定ビルドの起動ログから取ること」）。
fn capture_serial_for_calibration(run: usize) -> Result<String> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, &[])?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join(format!("serial-calibration-{run}.log"));
    let _ = fs::remove_file(&serial_log);

    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);
    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        // **既定は TCG である。** KVM での較正結果は測っていない。
        accelerator: Accelerator::Tcg,
        debug_events: DebugEvents::IntAndCpuReset,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the calibration run")?;

    // 較正が出るまで待つ。**上限を必ず付ける**（CLAUDE.md の「シェルコマンドの制約」）。
    let deadline = Instant::now() + CALIBRATION_RUN_TIMEOUT;
    loop {
        if read_lossy(&serial_log).contains("apic: LAPIC timer calibration:") {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();

    Ok(read_lossy(&serial_log))
}

/// 較正 1 回あたりの上限。較正窓は 5 標本 × 100ms なので、起動と合わせて余裕を取る。
const CALIBRATION_RUN_TIMEOUT: Duration = Duration::from_secs(40);

/// I/O APIC のレジスタが実際にデコードされることの確認（S2-a）。
///
/// **既定ビルドである**（`feature` が空）。壊すのではなく、読み経路が生きている
/// ことを見る。S1-c の時点では翻訳が張られたことしか確かめられておらず、
/// 「MMIO が本当にデコードされるか」は未確認のまま残っていた。S2-a で IOREGSEL
/// への書き込みを解禁して読めるようになったので、ここで閉じる。
///
/// **生の値をマーカーにしない。** `0x00170020` のような実測値そのものを書くと、
/// 構成が変わったときに「何を主張していた検査なのか」が読めないまま落ちる。
/// 主張が読める文言（デコードしていること、エントリが 24 本あること）に一致を取る。
///
/// 禁止マーカーは、判定が否側へ落ちたときにだけ出る行である。
const APIC_DECODE_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "ioapic-decodes",
    feature: "",
    expected_markers: &[
        "apic: I/O APIC MMIO decodes",
        "24 redirection entr(y/ies)",
        // **定常状態まで到達すること。** この行が無いと、上の 2 つが出た時点で
        // 打ち切るので**その後で起動が止まる退行を捕まえられない**（`-smp 2` の
        // 項目で実際に踏んだ形と同じ死角である）。既定ビルドを走らせる項目は
        // これを入れておく。`APIC_TESTS` が同じ理由で入れてあるのに揃える。
        "heartbeat: ticks=",
    ],
    forbidden_markers: &["does not look decoded"],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

/// APIC MMIO の写像の破壊確認（S1-c）。
///
/// **観測は panic ではない。** `acpi` と同じく S1 の経路は異常を見つけても
/// 停止しないので、「検出のログが出ること」と「Local APIC の読みが行われない
/// こと」の 2 つで判定する。ハートビートを期待マーカーに入れて、**検出した後も
/// カーネルが動き続けること**まで見る。
///
/// 禁止マーカーの `apic: LAPIC probe:` は、**写像が確認できたときにだけ出る行**
/// である。検出をすり抜けた場合にこれが出る。
///
/// **3 種は検出経路が別である。** 写像の有無 / 写像先の正しさ / MSR との
/// 突き合わせで、1 つの破壊で複数の経路が同時に落ちない形にしてある。
const APIC_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "skip-map",
        feature: "apic-test-skip-map",
        expected_markers: &[
            "[apic-test-skip-map] skipping the mapping",
            "still has no translation",
            "registers are NOT read",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["apic: LAPIC probe:"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "wrong-target",
        feature: "apic-test-wrong-target",
        // **「翻訳がある」だけでは通らないことを、この文言で示す。** 翻訳は
        // 張られているので、`still has no translation` は出ない。
        expected_markers: &[
            "[apic-test-wrong-target] pointing the mapping",
            "not the expected physical address",
            "registers are NOT read",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["apic: LAPIC probe:", "still has no translation"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "base-mismatch",
        feature: "apic-test-base-mismatch",
        // 突き合わせで止まるので、**写像そのものへ到達しない。**
        expected_markers: &[
            "[apic-test-base-mismatch] moving the MADT local APIC address",
            "the two disagree, so nothing is mapped and nothing is read",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &[
            "apic: LAPIC probe:",
            "apic: mapped the local APIC",
            "the sabotage did nothing",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// **構造的なガードが既定ビルドのバイナリに在ること**を見る（S4-c-3-2b、S5-bで拡張）。
///
/// **名前は主張を指す。** 当初は検出器だけを見ていたが、**世代フラッシュ（S5-b）も
/// 同じ問いなので同じ検査へ入れた。** **「検出器のシンボル」では主張とずれる。**
///
/// # なぜこの検査が要るのか
///
/// 検出器は**本番ビルドにも置いてある。** 守りが 2 層とも効いている限り鳴らない
/// ので、**「鳴らないこと」が主張になる。** ところが**コードが無くても鳴らない。**
/// 「本番で出ない」と「コードが無い」を区別するために、シンボルの存在を見る。
///
/// # **主たる論拠は構造の側にある。これは裏取りである**
///
/// 破壊 `sched-ignore-current` が触るのは `pick_next` のフィルタでの参照だけで、
/// **検出器の呼び出しに `cfg` は付かない。** よって検出器は構成によらず全ビルドに
/// 在る。**この検査はそれを機械的に確かめるだけである。**
///
/// # 世代フラッシュも同じ問いである（S5-b）
///
/// `bkl` の世代フラッシュは、**探りが feature 越しなので、既定ビルドに在ることを
/// 探りでは示せない。** 検出器と同じく、**主たる論拠は構造の側**（`cfg` が付かない）
/// で、**ここはその裏取りである。**
///
/// # **限界: 「呼ばれうる位置に在る」までしか見えない**
///
/// シンボルが在ることは、**正しい位置で呼ばれることを示さない。** それを示すのは
/// 2 層とも壊した構成（`smp-ap-test sched-ignore-both-layers`）で実際に鳴るほうで
/// ある。**この検査だけが緑でも、検出器が働いていることの証明にはならない。**
/// **世代フラッシュも同じで、「正しい位置で呼ばれる」ことは
/// `smp-ap-test tlb-generation` と `tlb-shootdown` が示す。**
fn check_structural_guard_symbols_present(workspace_root: &Path) -> Result<String> {
    let kernel_elf = build_kernel(workspace_root, false)?;
    let output = external_tool("nm")
        .arg(&kernel_elf.elf)
        .output()
        .context("failed to invoke nm (is binutils installed?)")?;
    if !output.status.success() {
        bail!("nm failed on {}", kernel_elf.elf.display());
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut found = Vec::new();
    for fragment in STRUCTURAL_GUARD_SYMBOL_FRAGMENTS {
        match listing.lines().find(|line| line.contains(fragment)) {
            Some(line) => found.push(line.split_whitespace().last().unwrap_or(line).to_string()),
            None => bail!(
                "the default kernel build has no symbol containing {fragment:?}. The production \
                 guards must exist in the default build: \"it does not fire\" is only a claim \
                 if the code is there. If it was renamed, update \
                 STRUCTURAL_GUARD_SYMBOL_FRAGMENTS; if it was removed or inlined away, restore it (both are marked #[inline(never)] for \
                 exactly this reason)."
            ),
        }
    }
    Ok(found.join(", "))
}

/// 埋め込む ext2 の像が `e2fsck -fn` を通ること（S10-a）。
///
/// # 何を捕まえる検査か
///
/// **落ちるのは「像の作り手が変わった」ときである。** `kernel/build.rs` は
/// `mke2fs` で像を建て、**そのあと時刻の 7 箇所をゼロで上書きしている。**
/// 上書きは像のバイトを直接書き換える操作なので、**別の版の `mke2fs` が
/// 別の場所に別の大きさで時刻を置けば、無関係なバイトを潰しうる。**
/// そのとき像は壊れるが、**こちらのパーサは壊れた側を読んでも気づかない**
/// ——superblock と inode は形として妥当なままだからである。
///
/// **判定行に載せた `mke2fs` の版と対になる検査である。** あちらは
/// 「作り手が変わったこと」を人が読んで気づくための記録で、こちらは
/// 「作り手が変わって像が壊れたこと」を機械で止める側である。
///
/// # 依存はもう及んでいる
///
/// **e2fsprogs はビルドの要求に既に入っている**（`mke2fs` が無ければ
/// `kernel/build.rs` が止まる。`ADR-0025`）。`e2fsck` は同じパッケージなので、
/// **この検査は新しい要求を足していない。** S12（書き込みの独立検証）で
/// どのみち必須になる。
///
/// # `OUT_DIR` を `find` で拾わない
///
/// **`OUT_DIR` は feature 構成ごとに別である。** `target/` を探して 1 つ拾うと、
/// 破壊ビルドの残骸を掴みうる（`--full` の直後がその状態になる）。**cargo に
/// 訊く**——`--message-format=json` の `build-script-executed` が、
/// **いま建てた構成の `out_dir` をパッケージごとに 1 行で返す。**
///
/// # ここが見るのは既定構成の像である
///
/// **[`kernel_build_out_dir`] は `--features` を付けずに建て直す**ので、
/// **返るのは常に既定構成の `OUT_DIR` である。** **この検査の対象は既定構成の
/// 像なので、それでよい**——**意図して既定を見ている、と書いておく。**
/// **同じ形が別の場所では誤りだった**（`stage_esp` が破壊ビルドへ既定の像を
/// 載せていた。`docs/troubleshooting.md`）。**構成を渡す側と訊く側が分かれて
/// いる形は、意図か誤りかを毎回書き分けること。**
/// 像へ入れるテキストが ASCII だけであることを見る（2026-08-31。文字化けの手当て）。
///
/// # なぜ ASCII に限るのか
///
/// **フォントが収録しているのは可読 ASCII（`0x20..=0x7E`）と置換文字だけである**
/// （`kernel/src/graphics/font/`。実測）。**それ以外の字は U+FFFD の箱になる。**
/// **`/etc/environment` の日本語のコメントが、画面で箱の列に見えていた**
/// （運用者の報告。2026-08-31）。**装置の中身は壊れていない**——`debugfs` が
/// 読んだバイト列は種のものと一致した。**壊れていたのは見え方だけである。**
///
/// # 見るのは種の木だけである
///
/// **`kernel/fsimage/seed/` の下は、人が書いて像へ入るテキストである。**
/// **`/bin` の実行ファイルは機械語なので対象にしない**——**実測で 0x80 以上の
/// バイトを普通に含む**（`zash` は 3831 バイト。2026-08-31）。
///
/// **`build.rs` が像へ書くもの（`/data/*`）は見ていない。** **いまはどれも
/// ASCII か、機械が読むだけの模様である**（実測。2026-08-31）。**人が読む
/// テキストをあちらへ足すなら、この検査の範囲を広げること。**
///
/// # 公開前の監査とは別物である
///
/// **あちらは追跡下の全ファイルの非 ASCII を Unicode ブロック別に数え、
/// 日本語が在ることを前提にしている**（`docs/verification-coverage.md` の
/// 「公開前の監査」）。**こちらは像の中のテキストに ASCII を要求する。**
/// **対象も判定も違うので、二重に持ったことにはならない。**
fn check_image_text_is_ascii(workspace_root: &Path) -> Result<String> {
    let root = workspace_root.join(IMAGE_TEXT_ROOT);
    if !root.is_dir() {
        bail!(
            "xtask check: {IMAGE_TEXT_ROOT} is not a directory ({}). If the seed tree moved, \
             update IMAGE_TEXT_ROOT in xtask along with it",
            root.display()
        );
    }

    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(&root, &mut files)?;
    files.sort();

    let mut bytes = 0usize;
    for file in &files {
        let content = fs::read(file)
            .with_context(|| format!("could not read {} for the ASCII check", file.display()))?;
        bytes += content.len();
        // **落ちる位置を行と桁で言う。** 直す人が開く先はエディタである。
        let mut line = 1usize;
        let mut column = 1usize;
        for byte in &content {
            if *byte >= 0x80 {
                let shown = file
                    .strip_prefix(workspace_root)
                    .unwrap_or(file.as_path())
                    .display();
                bail!(
                    "xtask check: {shown} holds the non-ASCII byte {byte:#04x} at line {line}, \
                     column {column}. Text that goes into the image must be ASCII: the console \
                     font holds printable ASCII and the replacement character, so anything else \
                     renders as a box (docs/coding-standards.md, the section named \
                     「像へ入れるテキストはASCIIに限る」)"
                );
            }
            if *byte == b'\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
    }

    Ok(format!(
        "{} file(s) under {IMAGE_TEXT_ROOT}/, {bytes} byte(s), every byte below 0x80",
        files.len()
    ))
}

/// 像へ入るテキストの置き場（2026-08-31）。
const IMAGE_TEXT_ROOT: &str = "kernel/fsimage/seed";

/// カーネルが XMM の命令を 1 つも持たないことを見る（`ADR-0058` の Decision 5）。
///
/// # なぜこれが要るのか
///
/// **「カーネルへ入って同じタスクへ戻るだけなら FP の退避が要らない」の根拠が、
/// これだからである。** **カーネルがうっかり浮動小数点を使うと、その前提が
/// 黙って崩れる**——**ユーザーの XMM がシステムコールの中で壊れ、しかも
/// どの判定も鳴らない。**
///
/// # 既定の構成だけを見る
///
/// **feature で入る検査用のコードは対象外である。** 見るのは
/// `cargo build -p kernel` が作る既定の像で、**そこに XMM が現れないこと。**
///
/// # 落とす破壊
///
/// **カーネルへ XMM の命令を 1 つ入れると落ちる。** **置くときに一度作って
/// 確かめた**（`docs/coding-standards.md` の「新しい静的な検査は、主張が偽の
/// 状態を一度作って落ちることを確かめてから置く」）。
fn check_kernel_has_no_xmm(workspace_root: &Path) -> Result<String> {
    let kernel = build_kernel_with_features(workspace_root, &[])?;
    let output = external_tool("objdump")
        .arg("-d")
        .arg(&kernel.elf)
        .output()
        .context("failed to run objdump on the kernel")?;
    if !output.status.success() {
        bail!("objdump failed on {}", kernel.elf.display());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut sites: Vec<String> = Vec::new();
    for line in text.lines() {
        // **レジスタ名で見る。** `fxsave` / `fxrstor` / `ldmxcsr` は当たらない
        // ——**あれらは XMM の状態を丸ごと動かす命令で、レジスタを名指ししない。**
        let uses_xmm = line
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| {
                word.starts_with("xmm")
                    && word[3..].chars().all(|c| c.is_ascii_digit())
                    && word.len() > 3
            });
        if uses_xmm {
            sites.push(line.trim().to_string());
        }
    }
    if !sites.is_empty() {
        let shown: Vec<String> = sites.iter().take(3).cloned().collect();
        bail!(
            "the kernel uses {} XMM register(s) ({}{}). ADR-0058 Decision 5 rests on the kernel \
             never touching FP state: if it does, a syscall clobbers the caller's registers and \
             nothing catches it",
            sites.len(),
            shown.join(" | "),
            if sites.len() > shown.len() {
                " | ..."
            } else {
                ""
            }
        );
    }
    Ok(format!(
        "no XMM register appears in {} disassembled line(s) of the default kernel",
        text.lines().count()
    ))
}

/// `root` の下のファイルを再帰で集める（2026-08-31）。
fn collect_files(root: &Path, into: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)
        .with_context(|| format!("could not read the directory {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("could not walk {}", root.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, into)?;
        } else {
            into.push(path);
        }
    }
    Ok(())
}

fn check_fs_image_passes_e2fsck(workspace_root: &Path) -> Result<String> {
    let out_dir = kernel_build_out_dir(workspace_root)?;
    let image = out_dir.join(FS_IMAGE_NAME);
    // **この分岐は今日の構成では届かない**（実測）。像が無ければ `include_bytes!` が
    // 先に落ち、カーネルのビルドが失敗する。**`build.rs` が置き場所を変えたときの
    // ためだけに残してある**——そのとき e2fsck の「そんなファイルは無い」より、
    // どこを探したかが出るほうが早い。**届かないことを承知で置いていると書く。**
    if !image.exists() {
        bail!(
            "the kernel build script reported an OUT_DIR without {}: {}",
            FS_IMAGE_NAME,
            image.display()
        );
    }

    let summary = run_e2fsck(&image)?;
    let constants = fs_image_constants(&out_dir)?;
    Ok(format!("{summary}; build.rs measured {constants}"))
}

/// 像に「どこで・誰が建てたか」が残っていないことを見る（2026-09-06、静的）。
///
/// # 何を主張するか
///
/// **同じ木からは、誰がどこで建てても同じ像が出ること。** 見るのは 2 つである。
///
/// - **建てた場所**——像のどこにも作業ツリーの絶対パスが現れないこと
/// - **建てた人**——全 inode の `i_uid` / `i_gid`（と上位半分）が 0 であること
///
/// # 実測で 2 つとも出た（2026-09-06）
///
/// **CI へ `--commit` を足した 1 回目と 2 回目が、どちらもこれで赤になった。**
/// **`rustc` へ原本を絶対パスで渡していたので `panic` の位置が像へ載り、
/// `mke2fs -d` が種の所有者を写していたので uid が像へ載っていた**
/// （`docs/troubleshooting.md` の 2026-09-06）。
///
/// # ここが見るのは 4 つの軸のうち 2 つである
///
/// **一覧は `kernel/build.rs` の `build_fs_image` の doc にある**（時刻 / 場所 /
/// 人 / 順序）。**この検査は場所と人を見る。** **時刻は建てた後に潰してあり、
/// 順序は `mke2fs` が名前で並べるので依っていない**（どちらも実測）。
///
/// # 起動ログの差分では代わりにならない
///
/// **あちらでも出る**（像の checksum が参照と違う）。**ただし 2 つの弱点がある。**
///
/// - **「checksum が違う」としか言わない。** 何が混ざったかは、像を取り出して
///   突き合わせるまで分からない（実際にそうやって絞った）
/// - **参照を録り直すと消える。** **録り直した人の uid が参照へ入るだけで、
///   その人の手元だけが緑になる**——**この検査は参照に依らない。**
///
/// # 落とす破壊
///
/// **2 つとも、一度作って落ちることを確かめた**（`docs/coding-standards.md` の
/// 「新しい静的な検査は、主張が偽の状態を一度作って落ちることを確かめてから
/// 置く」）。**`kernel/build.rs` の `--remap-path-prefix` を外すと場所の側が、
/// `zero_image_build_traces` の `i_uid` / `i_gid` の行を外すと人の側が落ちる。**
fn check_image_has_no_build_traces(workspace_root: &Path) -> Result<String> {
    let out_dir = kernel_build_out_dir(workspace_root)?;
    let image = out_dir.join(FS_IMAGE_NAME);
    let bytes = fs::read(&image)
        .with_context(|| format!("could not read the image {}", image.display()))?;

    // --- 建てた場所 ---
    let root = workspace_root.to_string_lossy().into_owned();
    let needle = root.as_bytes();
    if let Some(at) = bytes.windows(needle.len()).position(|w| w == needle) {
        bail!(
            "the image carries the absolute path of this working tree at byte {at} ({root}). \
             Something the build embeds is compiled with an absolute source path, so the image \
             differs per checkout directory (see kernel/build.rs, --remap-path-prefix)"
        );
    }

    // --- 建てた人 ---
    // **inode の位置の引き方は `kernel/build.rs` の `zero_image_build_traces`
    // と同じである。** **写しではあるが、片方が壊れたときにもう片方が落ちる
    // 形なので、同じ出所から引いてはならない。**
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32_at =
        |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    const SUPERBLOCK_OFFSET: usize = 1024;
    let sb = SUPERBLOCK_OFFSET;
    let block_size = 1024usize << u32_at(sb + 24);
    let inodes_count = u32_at(sb) as usize;
    let inodes_per_group = u32_at(sb + 40) as usize;
    let inode_size = u16_at(sb + 88) as usize;
    let first_data_block = u32_at(sb + 20) as usize;
    let group_count = inodes_count.div_ceil(inodes_per_group);
    let gd_table = (first_data_block + 1) * block_size;

    let mut owned: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for group in 0..group_count {
        let gd = gd_table + group * 32;
        let inode_table = u32_at(gd + 8) as usize * block_size;
        for index in 0..inodes_per_group {
            let at = inode_table + index * inode_size;
            if at + inode_size > bytes.len() {
                break;
            }
            seen += 1;
            let uid = u16_at(at + 2);
            let gid = u16_at(at + 24);
            let uid_high = u16_at(at + 120);
            let gid_high = u16_at(at + 122);
            if uid != 0 || gid != 0 || uid_high != 0 || gid_high != 0 {
                let number = group * inodes_per_group + index + 1;
                owned.push(format!(
                    "inode {number} is owned by {}:{}",
                    u32::from(uid) | (u32::from(uid_high) << 16),
                    u32::from(gid) | (u32::from(gid_high) << 16)
                ));
            }
        }
    }
    if !owned.is_empty() {
        let shown: Vec<String> = owned.iter().take(3).cloned().collect();
        bail!(
            "{} of {seen} inode(s) carry the owner of whoever built the image ({}{}). \
             mke2fs -d copies the seed files' owner; kernel/build.rs zeroes it after building",
            owned.len(),
            shown.join(", "),
            if owned.len() > shown.len() {
                ", ..."
            } else {
                ""
            }
        );
    }

    Ok(format!(
        "{seen} inode(s) owned by 0:0, and the image does not carry this working tree's path \
         ({} byte(s) scanned)",
        bytes.len()
    ))
}

/// `kernel/build.rs` が像から測って生成した定数の読み出し（e-5 の後の手当て）。
///
/// # なぜ判定行に出すのか
///
/// **e-4 で、像の中の番号を手で測る作業が消えた**——`build.rs` が `debugfs` へ
/// 訊いて定数を生成し、カーネルはそれを `include!` する。**手で写す誤りは
/// 根治したが、同時に人の目からも消えた。** **像がずれたとき、何がずれたかを
/// 言える先が要る**（運用者の要求）。
///
/// **実際に動いている**——`O_CREAT` の段で `/bin/zi` が太り、`MOTD_DATA_BLOCK`
/// が 92 から 93 へ動いた。**この行が無ければ、その番号はどこにも出ない。**
/// **像の番号に依る破壊（`corrupt` の族）が的を外したときの、最初の手掛かりである。**
///
/// # 一覧を手で持たない
///
/// **生成物の `pub const` を全部出す。** 4 つを名指しで並べると、**次に足された
/// 定数が黙って見えないままになる**——`docs/coding-standards.md` の
/// 「手で並べた一覧を数の出所にしない」と同じ形である。
///
/// # 検査ではない。読み出しである
///
/// **この関数は値を判定しない。** 期待値を持てば、それは
/// **`build.rs` が測った値を `xtask` へ書き写すことになり、e-4 が消した
/// 手作業が戻る。** **人が読んで気づく側**であって、機械で止める側ではない
/// （**止める側は同じ検査の `e2fsck` である**。`mke2fs` の版と対にしたのと
/// 同じ分担である）。
///
/// # ここが読むのは既定構成の生成物である
///
/// **[`kernel_build_out_dir`] が既定構成の `OUT_DIR` を返す**ので、出るのは
/// 既定構成の像の番号である。**意図してそうしている**——この検査の相手は
/// 既定構成の像で、`e2fsck` が見ているものと同じである。
fn fs_image_constants(out_dir: &Path) -> Result<String> {
    let path = out_dir.join(FS_IMAGE_INFO_NAME);
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read the constants generated by kernel/build.rs: {}",
            path.display()
        )
    })?;

    let mut pairs: Vec<String> = Vec::new();
    for line in text.lines() {
        // 生成されるのは `pub const NAME: TYPE = VALUE;` の 1 行だけである。
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, rest)) = rest.split_once(':') else {
            continue;
        };
        let Some((_, value)) = rest.split_once('=') else {
            continue;
        };
        pairs.push(format!(
            "{}={}",
            name.trim(),
            value.trim().trim_end_matches(';')
        ));
    }

    // **空で通さない。** 生成の形が変われば、この読み出しは黙って何も
    // 出さなくなる——**出ないことと、値が変わっていないことが区別できない。**
    if pairs.is_empty() {
        bail!(
            "{} carried no `pub const` line; kernel/build.rs changed the shape of what it \
             generates, and this readout stopped reporting anything",
            path.display()
        );
    }

    Ok(pairs.join(" "))
}

/// `e2fsck` の出力のうち、**不満ではない行**（S12-b）。
///
/// # なぜ「不満の一覧」ではなく「不満でない一覧」なのか
///
/// **想定外の不満を黙って通さないためである。** 不満の側を列挙すると、
/// **一覧に無い種類の不満が出たときに、それが不満だと分からない。**
/// **こちらを列挙して、残りをすべて不満として数える。**
///
/// # 「1 本」の数え方
///
/// **行数では数えない。** `e2fsck` は 1 つの不満に `Fix? no` を続けるので、
/// **1 つの不満が複数行になる。** **この一覧に当たらない行を 1 本と数える。**
///
/// # 文言に結合していることを承知で置く
///
/// **S12 の要は、外部の実装が判定することである**（`docs/roadmap.md` の S12）。
/// 自前で会計の欄を読めば版に依らなくなるが、**それは同じ設計で書いたものを
/// 同じ設計で読むことになり、外部性を失う。** **結合は払う費用である。**
///
/// **したがって 1 箇所に集める。** S12-c 以降も同じ出力を読むので、
/// **判定ごとに文字列を書き散らすと、版が変わったときに直す場所が段の数だけ増える。**
const E2FSCK_NOISE: &[&str] = &[
    "e2fsck ",
    "Pass 1:",
    "Pass 2:",
    "Pass 3:",
    "Pass 4:",
    "Pass 5:",
    "WARNING: Filesystem still has errors",
];

/// 不満の行の末尾に付く問い（S12-c）。
///
/// # `E2FSCK_NOISE` に入れてはならない
///
/// **`Fix? no` は単独の行にも、不満と同じ行の末尾にも出る。**
/// **`contains` で雑音として落とすと、同じ行に載った不満ごと消える。**
///
/// **実測で踏んだ**——`Inode 22, i_size is 100, should be 8192.  Fix? no` が
/// **まるごと落ち、判定が「不満 0 本」になっていた。** 破壊を有効にしたのに
/// 判定 A が通り、**判定 B だけが落ちた。**
/// **落ちる判定が 1 つ減っていたことに、破壊を走らせて初めて気づいた。**
///
/// **したがって、落とすのではなく末尾から剥がす。** 剥がした残りが空なら、
/// その行は問いだけだったということである。
const E2FSCK_FIX_PROMPT: &str = "Fix? no";

/// `e2fsck` の出力から、不満の行だけを取り出す（S12-b）。
///
/// **要約行（`… files (…), … blocks`）も落とす**——あれは結果であって不満ではない。
fn e2fsck_complaints(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !E2FSCK_NOISE.iter().any(|noise| line.contains(noise)))
        .filter(|line| !(line.contains(" files (") && line.contains(" blocks")))
        // **問いを末尾から剥がす。** 落とすと、同じ行に載った不満ごと消える。
        .map(|line| match line.split_once(E2FSCK_FIX_PROMPT) {
            Some((before, _)) => before.trim().to_string(),
            None => line.to_string(),
        })
        .filter(|line| !line.is_empty())
        .collect()
}

/// `e2fsck` の版（判定行に出す。S12-b）。
///
/// **`mke2fs` の版は像の行に出ているが、こちらは別に出す**——
/// **同じパッケージでも、ホストによっては違いうる。**
fn e2fsck_version() -> String {
    external_tool("e2fsck")
        .env("LC_ALL", "C")
        .arg("-V")
        .output()
        .ok()
        .map(|out| {
            let text = String::from_utf8_lossy(&out.stderr).to_string()
                + &String::from_utf8_lossy(&out.stdout);
            text.lines().next().unwrap_or("unknown").trim().to_string()
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// ある像へ `e2fsck -fn` を当て、不満の行を返す（S12-b）。
///
/// **落ちない。** 不満が在ることそのものが判定の材料なので、
/// **呼び出し側が数える**（[`e2fsck_complaints`]）。
fn e2fsck_complaint_lines(image: &Path) -> Result<Vec<String>> {
    let output = external_tool("e2fsck")
        .env("LC_ALL", "C")
        .arg("-fn")
        .arg(image)
        .output()
        .context(
            "failed to invoke e2fsck (it ships with e2fsprogs, the same package as mke2fs, \
             which the kernel build script already requires)",
        )?;
    Ok(e2fsck_complaints(&String::from_utf8_lossy(&output.stdout)))
}

/// ある像へ `e2fsck -fn` を当て、要約行を返す（S10-a。S12-a で寄せた）。
///
/// # 見る相手が 2 つある
///
/// **`build.rs` が建てた像**（S10-a）と、**ZaytOS が RAM に持っている像を
/// 取り出したもの**（S12-a）である。**同じ道具で、見る相手が違う。**
/// **寄せたのは、ガードページを 2 か所で張っていたのと同じ形を作らないためである**
/// （`kernel::stack::install_guard_page` の doc）。
fn run_e2fsck(image: &Path) -> Result<String> {
    // `-f` は clean でも全パスを走らせる（`s_state` を信用しない）。`-n` は
    // 何も直さず、直す必要があれば失敗で返す。**像を書き換えさせない。**
    // `LC_ALL=C` は要約行を言語設定に依らせないため（**この行を報告に載せる**）。
    let output = external_tool("e2fsck")
        .env("LC_ALL", "C")
        .arg("-fn")
        .arg(image)
        .output()
        .context(
            "failed to invoke e2fsck (it ships with e2fsprogs, the same package as mke2fs, \
             which the kernel build script already requires)",
        )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        bail!(
            "e2fsck rejected {} ({}). If this is the embedded image, it is built by mke2fs and \
             then has its timestamps zeroed in place by kernel/build.rs; a different e2fsprogs \
             version can put those timestamps elsewhere, in which case the overwrite corrupts \
             unrelated bytes. Compare the mke2fs version on the boot log's ext2 line.\n{}{}",
            image.display(),
            output.status,
            stdout,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 要約行（`<像>: N/M files (...), N/M blocks`）から、像のパスを落として返す。
    // **使用量が判定行に残り、パスに依らない**（`OUT_DIR` はハッシュを含む）。
    let summary = stdout
        .lines()
        .find(|line| line.contains(" files (") && line.contains(" blocks"))
        .and_then(|line| line.rsplit_once(": "))
        .map(|(_, counts)| counts.trim().to_string())
        .unwrap_or_else(|| "e2fsck printed no usage summary".to_string());
    Ok(summary)
}

/// `kernel/build.rs` が生成物を置いた `OUT_DIR`（既定の feature 構成）。
///
/// **cargo の JSON 出力から引く。** `serde` は入れない——見るのは
/// `build-script-executed` の行 1 種類で、必要な欄は 2 つだけである。
fn kernel_build_out_dir(workspace_root: &Path) -> Result<PathBuf> {
    let output = Command::new("cargo")
        .current_dir(workspace_root)
        .args([
            "build",
            "--target",
            KERNEL_TARGET,
            "-p",
            KERNEL_PACKAGE,
            "--bin",
            KERNEL_PACKAGE,
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .context("failed to invoke cargo to locate the kernel build script's OUT_DIR")?;
    if !output.status.success() {
        bail!(
            "kernel build failed while locating OUT_DIR ({})\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("\"reason\":\"build-script-executed\"") {
            continue;
        }
        // **kernel 以外のパッケージのビルドスクリプトも同じ形で出る。**
        if !line.contains(&format!("/{KERNEL_PACKAGE}#")) {
            continue;
        }
        if let Some(dir) = json_string_field(line, "out_dir") {
            return Ok(PathBuf::from(dir));
        }
    }
    bail!(
        "cargo did not report a build-script-executed message for {KERNEL_PACKAGE}; \
         cannot locate OUT_DIR"
    )
}

/// JSON の 1 行から `"name":"値"` の値を取り出す。
///
/// **エスケープを解かない。** cargo が返すのはパスであり、**この検査が扱う範囲では
/// `\` も `"` も現れない。** 現れたら開いたまま返るので、呼び出し側の
/// `exists()` が落ちる（**静かに別の場所を指すことはない**）。
fn json_string_field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("\"{name}\":\"");
    let start = line.find(&key)? + key.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// `kernel/build.rs` が `OUT_DIR` へ置く ext2 の像の名前。
const FS_IMAGE_NAME: &str = "fs.img";

/// `kernel/build.rs` が `OUT_DIR` へ置く、生成した定数の名前（e-5 の後の手当て）。
const FS_IMAGE_INFO_NAME: &str = "fsimage_info.rs";

/// **構造的なガード**のシンボル名に必ず現れる断片（S4-c-3-2b、S5-bで拡張）。
///
/// Rust のシンボルはマングルされるので、**関数名の断片で照合する。**
/// 改名したらここも直すこと（検査の失敗メッセージがそう言う）。
const STRUCTURAL_GUARD_SYMBOL_FRAGMENTS: &[&str] = &[
    "report_double_selection",
    "report_foreign_task_adoption",
    "report_layer_two_skip",
    // 世代フラッシュ（S5-b）。**探りは feature 越しなので、既定ビルドに在ることは
    // 探りでは示せない。** ここで裏取りする。
    "flush_if_generation_is_stale",
];

/// 意図的に壊した経路を有効にする feature の名前。
///
/// **接頭辞ではない。** 照合は完全一致である（`contains`）。**以前この doc は
/// 「接頭辞・名前」と書いていたが、コードは一度も接頭辞として扱っていない**（S6-e）。
///
/// **既定ビルドにこれらが入ってはならない。** 入ったまま出荷すると、
/// 壊れた状態で測った結果を正常な結果として扱うことになる。
const SABOTAGE_FEATURES: &[&str] = &[
    "percpu-fake-nonzero-cpu-id",
    "smp-tramp-corrupt-copy-test",
    "smp-ap-touch-scheduler-test",
    "lapic-timer-scale-calibration-test",
    "lapic-timer-wrong-divide-test",
    "lapic-timer-no-mask-all-test",
    "ioapic-wrong-vector-test",
    "ioapic-skip-unmask-test",
    "ioapic-keep-pic-irq1-test",
    "misalign-test",
    "idt-irq-stub-offset-test",
    "addrspace-no-kernel-share",
    "no-eoi-test",
    "alt-offset-test",
    "tiny-key-buffer",
    "paging-test",
    "exception-test",
    "critical-test",
    "interrupt-test",
    "panic-test",
    "gfx-test-pattern",
    "highhalf-no-identity-in-boot-pt",
    "highhalf-bad-high-slot",
    "highhalf-no-kernel-high-in-live-table",
    "highhalf-trampoline-absolute-ref",
    "highhalf-remove-verify-fail",
    "highhalf-remove-before-highify",
    "highhalf-panic-after-remove",
    "acpi-test",
    "apic-test",
    "smp-ap-no-sentinel-clear",
    "sched-ignore-owner",
    "sched-ignore-current",
    "smp-ap-runs-preemptive-demo",
    "sched-ignore-bootstrap-tripwire",
    "sched-keep-workers-runnable",
    "smp-ipi-probe",
    "smp-tlb-generation-probe",
    "smp-tlb-shootdown-probe",
    "smp-tlb-no-generation-bump",
];

/// 内部を隠す約束のディレクトリ。
///
/// - `kernel/src/irq/`: 割り込みコントローラとタイマ源の境界（S0-a）。外から
///   `pic` / `pit` を参照できないことをコンパイラが保証する。
/// - `kernel/src/task/`: スケジューラの実体（`static mut SCHEDULER`）。外から
///   構造体全体への参照を作れないことをコンパイラが保証する（S0-b）。
/// - `kernel/src/acpi/`: ファームウェアが提示する構成表の境界（S1-b）。
///   テーブルの生バイトとパーサの型は境界の中に留まる。**物理アドレスは
///   S1-c で出るようになった**（`ApicMmio`。APIC の MMIO を写像するには
///   所在そのものが要る）ので、「物理アドレスも留まる」はもう成り立たない。
///   宣言（`mod rsdp;`）は `acpi/mod.rs` にあるので、
///   `irq` と同じくディレクトリ指定で中に入る。
/// - `kernel/src/task.rs`: 上の `mod scheduler;` 宣言がここにある。**ディレクトリ
///   だけを見る形では、この 1 行が対象から外れる**（`irq` は宣言が
///   `irq/mod.rs` にあるので中に入るが、`task` は `task.rs` が外にある）。
///   実測で気づいた穴なので、ファイルを明示して対象に入れる。`task.rs` を
///   `task/mod.rs` へ改名すれば対称になるが、**このパスは docs から 6 箇所で
///   参照されている**（`verification-coverage` 4 / `roadmap` 1 /
///   `deferred-decisions` 1）ため改名しない。M5-f-3 を改名しなかったのと同じ
///   理由である。
///
/// `task` 側について 1 つ正確に書いておく。`mod scheduler;` を `pub mod` にしても、
/// アクセサが `pub(super) fn` なので外からは呼べない（実測では
/// `E0603: function switches is private` になった）。**この対象追加が捕まえるのは
/// 「漏れる形」ではなく「漏れる条件の片方」である。** `irq` の
/// `pub(crate) use super::pic::*;` と同じく、保守的に禁じている側に当たる。
/// 検査は `fn` の可視性を見ない（見ると境界の公開 API まで禁じることになる）。
///
/// **どちらも「コンパイラが保証し、検査はその保証が外されるのを防ぐ」形である。**
/// 保証の作り方が同じなので、検査も 1 つで足りる。
static PRIVATE_BOUNDARY_DIRS: &[&str] = &[
    "kernel/src/irq/",
    "kernel/src/task/",
    "kernel/src/task.rs",
    "kernel/src/acpi/",
];

/// 内部を隠す約束のモジュール（[`PRIVATE_BOUNDARY_DIRS`]）が、その内部を外へ
/// 公開していないことを確かめる（seam整備の項目1=S0-a、S0-b）。
///
/// # コンパイラが保証することと、この検査が守ること
///
/// `irq/mod.rs` が `mod pic;`（非公開）と宣言している限り、境界の外から
/// `crate::irq::pic::…` と書くとビルドが落ちる。**参照が無いこと自体は
/// コンパイラが保証する。** この検査が守るのは、その保証が将来の単純化で
/// 外されないことである。可視性修飾を 1 つ足すだけで保証は消えるが、
/// ビルドは通り続けるので、検査が無ければ気づけない。
///
/// # 見るもの
///
/// `irq/` 配下の**全ファイル**を対象に、`mod` 宣言と `use` に private 以外の
/// 可視性修飾（`pub` / `pub(crate)` / `pub(super)` / `pub(in …)`）が付いて
/// いないことを見る。`mod.rs` だけを見る形だと、配下に新しいファイルを作って
/// そこから再公開する経路を捕まえられない。
///
/// # 守らないもの（実態より強く書かない）
///
/// - **境界の公開関数が生の値を返す形は捕まらない。** `pub fn read_masks()
///   -> (u8, u8)` を `irq` に足せば、可視性は private のままでも呼び出し側は
///   PIC の語彙を持てる。これはレビューで守る。
/// - **コメント内の言及は対象外。** コメントは実行されないので結合を作らない。
///   `pic` は `topic` の部分文字列でもあり、素朴な一致は誤検出源になる。
/// - **生の I/O ポート直叩きは対象外。** 境界の外から `outb(0x21, …)` と書けば
///   IMR は触れる。現在そのような箇所は無いことを実測で確認しているが、
///   この検査はそれを見ていない。
///
/// # 保守的に禁じている形もある（実態より強く書かない）
///
/// 禁じている形のうち、**外から実際に到達できるものと、保守的に禁じている
/// だけのものが混ざっている。** 前者だけを見て「全部が穴だった」と読まない
/// ように、実測（S0-a の 1c）の結果をそのまま残す。
///
/// - **判定は行頭だけを見るが、インデントされた宣言を見逃す穴にはならない。**
///   インデントされるのはインライン `mod` の内側であり、その親が非公開なら
///   下と同じ理屈で外から到達できず、親が公開なら**親の行が行頭で拾われる**。
///   「行頭だけなのは手抜きでは」と考えて作り直すと、この性質が失われる。
/// - `pub(self) mod pic;` は実際には非公開と同義だが FAIL させる。これも下の
///   「保守的に禁じている形」に含まれる。
///
/// 実測（S0-a の 1c）の結果は次のとおり。
///
/// - `pub mod pic;` → 到達できる（`crate::irq::pic::MASK_ALL` がビルドを通る）
/// - `pub(crate) mod pic;` → クレート内から到達できる
/// - `irq/mod.rs` での `pub use pic::…` → 到達できる
/// - `irq/mod.rs` での `pub type Alias = pic::Inner;` → 到達できる（別名を通して
///   内部の型を掴める。`pub use` と同じ性質の実在の穴である）
/// - **配下の非公開ファイルからの `pub(crate) use super::pic::*;` → 到達できない。**
///   Rust の実効可視性は親モジュールの可視性で頭打ちになるので、
///   `crate::irq::compat::MASK_ALL` は `E0603: module compat is private` になる。
///   これは漏れる穴ではなく、**保守的に禁じているだけ**である。禁じたままに
///   するのは、`mod compat;` を `pub mod compat;` にするだけで漏れる形へ
///   変わるためで、その 1 文字の差を検査の外に置きたくない。
fn find_boundary_visibility_leaks(workspace_root: &Path) -> Result<Vec<String>> {
    // 列挙は SAFETY 検査と同じ理由で `--cached --others --exclude-standard`
    // にする。**未追跡の新規ファイルこそ検査が要る**（配下に新しいファイルを
    // 作って再公開する経路が、追跡される前に素通りするのを防ぐ）。
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "*.rs",
        ])
        .output()
        .context("failed to list Rust sources")?;
    if !output.status.success() {
        bail!("git ls-files failed while collecting Rust sources");
    }
    let listing = String::from_utf8(output.stdout).context("git ls-files produced non-UTF-8")?;

    let mut findings = Vec::new();
    for relative in listing.lines().filter(|l| !l.is_empty()) {
        if !PRIVATE_BOUNDARY_DIRS
            .iter()
            .any(|dir| relative.starts_with(dir))
        {
            continue;
        }
        let path = workspace_root.join(relative);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(item) = visibility_qualified_mod_or_use(trimmed) {
                findings.push(format!(
                    // 文言は境界の名前を書かない。対象は`PRIVATE_BOUNDARY_DIRS`で
                    // 増えるので、特定の境界（かつては割り込み層だけだった）を
                    // 名指しすると、対象が増えたときに事実と食い違う。
                    "{relative}:{}: {item} is exported out of a private-by-design boundary: {}",
                    index + 1,
                    trimmed.trim_end()
                ));
            }
        }
    }
    Ok(findings)
}

/// 行頭が「private 以外の可視性修飾 + `mod` / `use`」なら、その語を返す。
///
/// 判定は行の先頭だけを見る。`mod` と `use` はアイテム宣言なので、可視性修飾は
/// 必ず行頭に来る（インラインモジュールの中でも、その行の先頭に来る）。
fn visibility_qualified_mod_or_use(trimmed: &str) -> Option<&'static str> {
    let rest = trimmed.strip_prefix("pub")?;
    // `pub(crate)` / `pub(super)` / `pub(in …)` の括弧を読み飛ばす。
    let rest = match rest.strip_prefix('(') {
        Some(after) => after.split_once(')')?.1,
        None => rest,
    };
    let rest = rest.trim_start();
    if rest.starts_with("mod ") {
        Some("mod")
    } else if rest.starts_with("use ") {
        Some("use")
    } else if rest.starts_with("type ") {
        // `pub type Alias = pic::Inner;` は `mod` でも `use` でもないが、
        // **別名を通して内部の型を外から掴める**（実測で到達を確認した）。
        // `pub use` と同じ性質の実在の穴なので拾う。
        Some("type")
    } else {
        None
    }
}

/// kernel の既定 feature に仕込みが混ざっていないことを確かめる。
///
/// `default` から推移的に辿って、[`SABOTAGE_FEATURES`] のいずれかに
/// 行き着かないことを見る。`[features]` を そのまま読む
/// （`name = ["a", "b"]` の形しか使っていない）。
///
/// **kernel と bootloader の両方を見る（S6-e で広げた）。** 以前は kernel だけを
/// 読んでおり、**`panic-test`（bootloader の feature）は一覧にあっても照合の
/// 対象に一度も入っていなかった。** **bootloader の `default` が壊す feature へ
/// 行き着いても、誰も落とさない状態だった。**
///
/// **あわせて死んだエントリも見る**——[`SABOTAGE_FEATURES`] に、どちらの
/// マニフェストにも無い名前が載っていないこと。**許可リストの死んだエントリと
/// 同じ穴である**（S6-d）。
fn check_default_features_are_clean(workspace_root: &Path) -> Result<Vec<String>> {
    let mut findings = Vec::new();
    let mut declared: Vec<String> = Vec::new();
    for crate_name in ["kernel", "bootloader"] {
        findings.extend(check_one_manifest_default_features(
            workspace_root,
            crate_name,
            &mut declared,
        )?);
    }
    for feature in SABOTAGE_FEATURES {
        if !declared.iter().any(|d| d == feature) {
            findings.push(format!(
                "dead SABOTAGE_FEATURES entry (no such feature in kernel/ or bootloader/): \
                 `{feature}`"
            ));
        }
    }
    Ok(findings)
}

/// 1 つのマニフェストについて上を行う。宣言されている feature 名を `declared` へ足す。
fn check_one_manifest_default_features(
    workspace_root: &Path,
    crate_name: &str,
    declared: &mut Vec<String>,
) -> Result<Vec<String>> {
    let manifest = fs::read_to_string(workspace_root.join(crate_name).join("Cargo.toml"))
        .with_context(|| format!("failed to read {crate_name}/Cargo.toml"))?;

    let mut in_features = false;
    let mut graph: Vec<(String, Vec<String>)> = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_features = trimmed == "[features]";
            continue;
        }
        if !in_features || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, rest)) = trimmed.split_once('=') else {
            continue;
        };
        let deps: Vec<String> = rest
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|d| d.trim().trim_matches('"').to_string())
            .filter(|d| !d.is_empty())
            .collect();
        graph.push((name.trim().to_string(), deps));
    }
    declared.extend(graph.iter().map(|(name, _)| name.clone()));
    // **bootloader には `default` が無い。** 無い場合は「`default` から辿れる
    // ものは何も無い」ので、辿る側の検査は空で正しい。
    if !graph.iter().any(|(name, _)| name == "default") {
        return Ok(Vec::new());
    }

    // `default` から推移的に辿る。
    let mut reached: Vec<String> = vec!["default".to_string()];
    let mut index = 0;
    while index < reached.len() {
        let current = reached[index].clone();
        index += 1;
        if let Some((_, deps)) = graph.iter().find(|(name, _)| *name == current) {
            for dep in deps {
                if !reached.contains(dep) {
                    reached.push(dep.clone());
                }
            }
        }
    }

    Ok(reached
        .into_iter()
        .filter(|name| SABOTAGE_FEATURES.contains(&name.as_str()))
        .map(|name| format!("{crate_name}: `default` reaches the sabotage feature `{name}`"))
        .collect())
}

/// 並走している `xtask` / `qemu` を数える（2026-09-03）。
///
/// # なぜ機械で見るのか
///
/// **規律で守ろうとして、1 回目で失敗した。** **代償は 90 分と、信用できない
/// 131 件の合否である**（`docs/troubleshooting.md`）。**`ps` は打っていたが、
/// 起こすのと同じコマンドの中に書いたので、読む前に走り出していた。**
///
/// **この体制には前例がある**——**行末の `&` は hook で塞ぎ、`git add -A` も
/// 機械で拒む形にした。** **同じ族である。**
///
/// # 何を見るか
///
/// **`/proc` の `comm` を見る**（Linux。この体制は WSL2 の Ubuntu 系である）。
/// **`cargo` のロックは見ない**——**`cargo run` はビルドの間しか持たず、
/// 走り出した `xtask` は持っていない**（実測。**`--full` の最中に
/// `target/debug/.cargo-lock` は空いている**）。
///
/// # 自分を数えない
///
/// **自分の PID と、自分の親の PID を除く**（親は `cargo` だが、
/// 名前が変わる形に備えて除いておく）。**`ps` の部分一致で自分を拾う形は、
/// hook で 1 度踏んでいる**（`docs/troubleshooting.md` の 2026-08-28）。
///
/// # 逃げ道は置かない
///
/// **置くなら理由が要るが、思いつかない。** **並走させたい場面が無い**
/// ——**`target/` と `disk0.img` を共有するので、両方が汚れる**
/// （`CLAUDE.md` の絶対ルール 1）。**要るようになったら、そのとき足す。**
fn concurrent_build_or_qemu() -> Vec<(u32, String)> {
    let mut found = Vec::new();
    let me = std::process::id();
    let parent = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|stat| {
            // `pid (comm) state ppid ...`。**`comm` に空白が入りうるので `)` で切る。**
            let rest = stat.rsplit_once(')')?.1.to_string();
            rest.split_whitespace().nth(1)?.parse::<u32>().ok()
        })
        .unwrap_or(0);

    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|text| text.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me || pid == parent {
            continue;
        }
        let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        let comm = comm.trim().to_string();
        // **`comm` は 15 文字で切られる**ので、`qemu-system-x86_64` は
        // `qemu-system-x86` として出る。
        if comm == "xtask" || comm.starts_with("qemu-system-") {
            found.push((pid, comm));
        }
    }
    found.sort();
    found
}

/// 並走していたら断る（2026-09-03）。
///
/// **`--full` と `--commit` の入口で呼ぶ。** **基底の `check` では呼ばない**
/// ——**数秒で終わり、QEMU も起こさないので、並走しても汚れない**
/// （**コミット直後の hook がそれを回す**）。
fn refuse_if_something_else_is_running(what: &str) -> Result<()> {
    let others = concurrent_build_or_qemu();
    if others.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = others
        .iter()
        .map(|(pid, comm)| format!("{comm} (pid {pid})"))
        .collect();
    bail!(
        "xtask check {what}: another build or QEMU is running: {}. They share target/ and \
         disk0.img, so running both dirties each other (CLAUDE.md, absolute rule 1). Stop them \
         first: read .claude/skills/stop-a-process/SKILL.md, then kill the pid(s) above",
        list.join(", ")
    );
}

/// 全構成のビルド・テスト・clippy・fmt を順に実行する。
///
/// **1 つ落ちてもそこで止めない。** 止めると「直しては再実行」を
/// 繰り返すことになり、全体像が分からない。最後にまとめて報告する。
fn cmd_check(full: bool, commit: bool, update_reference: bool) -> Result<()> {
    // 外した確率的な項目の一覧が実態を指しているかを先に見る（列挙の腐りを防ぐ）。
    check_flaky_list_matches_tables()?;

    // **並走を機械で断る（2026-09-03）。** **規律で守ろうとして 1 回目で失敗した**
    // （[`refuse_if_something_else_is_running`] の doc）。
    if full {
        refuse_if_something_else_is_running("--full")?;
    } else if commit {
        refuse_if_something_else_is_running("--commit")?;
    }

    let workspace_root = workspace_root()?;
    let mut failed = Failures::default();
    let mut total = 0usize;
    // **`--full` にだけ上限を置く**（[`FULL_TIME_LIMIT`] の doc）。
    // **`base` と `--commit` は数分で終わるので、置く理由が無い。**
    if full {
        if let Ok(mut limit) = TIME_LIMIT.lock() {
            *limit = Some((Instant::now(), FULL_TIME_LIMIT));
        }
    }

    for (name, args) in CHECKS {
        total += 1;
        begin_item(name);
        // **ホストテストの本数を記録へ残す（2026-08-30）。**
        //
        // **固定はしない。** **実測で決めた**——**`#[test]` を足すか消した
        // コミットは 87 本、`EXPECTED_CHECK_COUNT` を触ったコミットは 13 本
        // である**（全 750 本のうち）。**固定すると 6.7 倍の手間が掛かる。**
        //
        // **それに、固定しても今回の欠陥は捕まらない**——**走らなくなった
        // 1 本と、同じコミットで足した 1 本で、合計が動かなかった**（実測）。
        // **捕まえたのは `--all-targets` を付けた clippy のほうである。**
        //
        // **数は主張しない。出すだけである**——**報告に残るので、
        // 減ったときに人が気づける。**
        if *name == "test (host)" {
            let output = Command::new("cargo")
                .current_dir(&workspace_root)
                .args(*args)
                .output()
                .with_context(|| format!("failed to invoke cargo for the {name} check"))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            print!("{stdout}");
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            let ran: u64 = stdout
                .lines()
                .filter_map(|line| line.strip_prefix("test result: ok. "))
                .filter_map(|rest| rest.split(' ').next()?.parse::<u64>().ok())
                .sum();
            if output.status.success() {
                println!(
                    "--- {name}: OK ({ran} host test(s) ran; the count is reported, not enforced)"
                );
            } else {
                println!("--- {name}: FAILED ({})", output.status);
                failed.push((*name).to_string());
            }
            continue;
        }
        let status = Command::new("cargo")
            .current_dir(&workspace_root)
            .args(*args)
            .status()
            .with_context(|| format!("failed to invoke cargo for the {name} check"))?;
        if status.success() {
            println!("--- {name}: OK");
        } else {
            println!("--- {name}: FAILED ({status})");
            failed.push((*name).to_string());
        }
    }

    total += 1;
    begin_item("the host test names match the reference");
    match check_host_test_names(&workspace_root, update_reference) {
        Ok(message) => println!("--- host test names: {message}"),
        Err(error) => {
            println!("--- host test names: FAILED ({error})");
            failed.push("host test names".to_string());
        }
    }

    total += 1;
    begin_item("unsafe blocks carry a SAFETY comment");
    let missing = find_unsafe_without_safety_comment(&workspace_root)?;
    if missing.is_empty() {
        println!("--- unsafe/SAFETY: OK");
    } else {
        for finding in &missing {
            println!("    {finding}");
        }
        println!("--- unsafe/SAFETY: FAILED ({} block(s))", missing.len());
        failed.push("unsafe/SAFETY".to_string());
    }

    total += 1;
    begin_item("direct cli/sti stays on the approved list");
    let mut approved_occurrences = 0usize;
    let unapproved = find_unapproved_interrupt_control(&workspace_root, &mut approved_occurrences)?;
    if unapproved.is_empty() {
        println!(
            "--- direct cli/sti: OK ({} approved entr(y/ies) = file+item pairs, covering {} \
             occurrence(s) = cli/sti lines)",
            DIRECT_INTERRUPT_CONTROL_ALLOWLIST.len(),
            approved_occurrences
        );
    } else {
        for finding in &unapproved {
            println!("    {finding}");
        }
        println!("    approved sites (file / item / reason):");
        for site in DIRECT_INTERRUPT_CONTROL_ALLOWLIST {
            println!("      {} / {} / {}", site.file, site.item, site.reason);
        }
        println!(
            "--- direct cli/sti: FAILED ({} unapproved site(s); exclusion belongs behind \
             InterruptGuard/Locked, or add an entry with a reason)",
            unapproved.len()
        );
        failed.push("direct cli/sti".to_string());
    }

    // **`--commit` はここで終わる**——基底 + boot log diff の 1 項目。
    // カーネルのコードに触れたコミットの前に回す（`docs/coding-standards.md` の
    // 「回帰チェックの必須条件」）。起動ログの参照が古いままコミットされる形
    // （S13-e-1 で実際に起きた）を、コミットの時点で止めるための段である。
    if full || commit {
        total += 1;
        begin_item("the boot log matches the reference and does not depend on the core count");
        match cmd_boot_log_diff(false) {
            Ok(()) => println!("--- boot log diff: OK"),
            Err(error) => {
                println!("--- boot log diff: FAILED ({error})");
                failed.push("boot log diff".to_string());
            }
        }
    }

    if full {
        // **シェルへ打鍵を送る（S11-11）。** **破壊ではない**——
        // **打鍵が Ring 3 まで届き、組み込みの `exit` が効き、`init` が
        // 起こし直すところまでを見る。**
        // **既定の起動ログには入れていない**（`sendkey` はタイミングに依存する）。
        // **3 回連続で通ることを確かめてから入れた。落ちる回が出たら `flaky` へ移す。**
        // **多バイトの字が画面と `zi` で正しく扱われること（`ADR-0054`）。**
        //
        // **1 回の起動で 10 見る**——**壊れたバイトが行を消さない / 全角が
        // 2 セル / 画面の桁が字で進む / 消すのが字である**（`ADR-0054`）、
        // **`$` が行末の字へ動く / `^` が空白を飛ばす / Esc が境界へ戻る /
        // 1 行目が字を保つ / `o` が下に行を開く**（VIM-1）。
        //
        // **落ちる判定は破壊ごとに違う**——**一覧は
        // [`UTF8_TEST_SABOTAGES`] と `docs/verification-coverage.md` にある。**
        total += 1;
        begin_item("multibyte characters take two cells and are edited whole");
        match cmd_utf8_test(&[], true) {
            Ok(()) => println!("--- utf8 test: OK"),
            Err(error) => {
                println!("--- utf8 test: FAILED ({error})");
                failed.push("utf8 test".to_string());
            }
        }
        for sabotage in UTF8_TEST_SABOTAGES {
            total += 1;
            let label = format!("utf8-test {sabotage}");
            begin_item(&label);
            match cmd_utf8_test(&[sabotage], false) {
                Ok(()) => println!("--- {label}: OK"),
                Err(error) => {
                    println!("--- {label}: FAILED ({error})");
                    failed.push(label.to_string());
                }
            }
        }

        // **起動時の設定が走ること（PR-1）。**
        //
        // **1 回の起動で 3 つ見る**——**2 行目まで走って `$NAME` が展開される /
        // 後のほうが勝つ / 無いときは何も言わない。**
        // **破壊は 3 つで、落ちる判定が 1 本ずつ違う**（実測。2026-09-04）。
        //
        // **`--shell-test` へ足していない**——**あちらは `sendkey` で
        // 1 キーずつ打つので、同じ主張が 12 構成に掛かって高い**
        // （`kernel/src/input.rs` の `profile-test` の台本の doc）。
        total += 1;
        begin_item("the shell runs /etc/profile and ~/.profile at start");
        match cmd_profile_test(&[], true) {
            Ok(()) => println!("--- profile test: OK"),
            Err(error) => {
                println!("--- profile test: FAILED ({error})");
                failed.push("profile test".to_string());
            }
        }
        for sabotage in PROFILE_TEST_SABOTAGES {
            total += 1;
            let label = format!("profile-test {sabotage}");
            begin_item(&label);
            match cmd_profile_test(&[sabotage], false) {
                Ok(()) => println!("--- {label}: OK"),
                Err(error) => {
                    println!("--- {label}: FAILED ({error})");
                    failed.push(label.to_string());
                }
            }
        }

        // **履歴がファイルで持ち越されること（HI-1）。**
        //
        // **1 回の起動で 3 つ見る**——**前の起動で打った行が辿って戻る /
        // ファイルが古い順である / 無いときは何も言わない。**
        // **破壊は 2 つで、落ちる判定が 1 本ずつ違う**（実測。2026-09-04）。
        total += 1;
        begin_item("the shell keeps its history in a file");
        match cmd_history_test(&[], true) {
            Ok(()) => println!("--- history test: OK"),
            Err(error) => {
                println!("--- history test: FAILED ({error})");
                failed.push("history test".to_string());
            }
        }
        for sabotage in HISTORY_TEST_SABOTAGES {
            total += 1;
            let label = format!("history-test {sabotage}");
            begin_item(&label);
            match cmd_history_test(&[sabotage], false) {
                Ok(()) => println!("--- {label}: OK"),
                Err(error) => {
                    println!("--- {label}: FAILED ({error})");
                    failed.push(label.to_string());
                }
            }
        }

        // **Tab の補完（TAB-1）。**
        //
        // **1 回の起動で 5 つ見る**——**単一候補 / 共通接頭辞 / 件数 / 一覧 /
        // `PATH` に従うこと**（重複は 2 本目の件数で見る）。
        // **破壊は 4 つで、落ちる判定が 1 本ずつ違う。**
        total += 1;
        begin_item("tab completes the word at the cursor");
        match cmd_complete_test(&[], true) {
            Ok(()) => println!("--- complete test: OK"),
            Err(error) => {
                println!("--- complete test: FAILED ({error})");
                failed.push("complete test".to_string());
            }
        }
        for sabotage in COMPLETE_TEST_SABOTAGES {
            total += 1;
            let label = format!("complete-test {sabotage}");
            begin_item(&label);
            match cmd_complete_test(&[sabotage], false) {
                Ok(()) => println!("--- {label}: OK"),
                Err(error) => {
                    println!("--- {label}: FAILED ({error})");
                    failed.push(label.to_string());
                }
            }
        }

        // **FP の状態（B-a。`ADR-0058`）。**
        //
        // **1 回の起動で 3 つ見る**——**起こされた時点の XMM が 0（決定 4）/
        // 足し上げが期待値と一致する（決定 1）/ `spawn` を跨いで親の XMM が
        // 残る（決定 2）。** **破壊は 2 つで、落ちる判定が 1 本ずつ違う。**
        total += 1;
        begin_item("floating-point state survives programs and spawn");
        match cmd_fp_test(&[], true) {
            Ok(()) => println!("--- fp test: OK"),
            Err(error) => {
                println!("--- fp test: FAILED ({error})");
                failed.push("fp test".to_string());
            }
        }
        for sabotage in FP_TEST_SABOTAGES {
            total += 1;
            let label = format!("fp-test {sabotage}");
            begin_item(&label);
            match cmd_fp_test(&[sabotage], false) {
                Ok(()) => println!("--- {label}: OK"),
                Err(error) => {
                    println!("--- {label}: FAILED ({error})");
                    failed.push(label.to_string());
                }
            }
        }

        total += 1;
        begin_item("the shell takes keystrokes and init restarts it");
        match cmd_shell_test(ShellTestMode::Normal) {
            Ok(()) => println!("--- shell test: OK"),
            Err(error) => {
                println!("--- shell test: FAILED ({error})");
                failed.push("shell test".to_string());
            }
        }

        // **矢印を落とすと挿入点が動かなくなること（S12 前の手当て）。**
        // **上の項目が主張していることの反証である**——**上は「動いた」を
        // 見ているが、動いていない像でも同じ判定が真になる形だと意味が無い。**
        // **打鍵を流す仕組みは上と同じもので、期待だけが裏返る**
        // （`ShellTestMode`）。
        total += 1;
        begin_item("dropping the arrows stops the insertion point from moving");
        match cmd_shell_test(ShellTestMode::ArrowsDropped) {
            Ok(()) => println!("--- shell test (arrows dropped): OK"),
            Err(error) => {
                println!("--- shell test (arrows dropped): FAILED ({error})");
                failed.push("shell test (arrows dropped)".to_string());
            }
        }

        // **Esc を落とすと実打鍵の Esc `[` `D` が CSI にならないこと（zi-a）。**
        // 上の矢印の破壊と同じ形——**通常の側の「Esc が届いた」判定の反証である。**
        total += 1;
        begin_item("dropping Esc keeps the literal Esc [ D keystrokes as characters");
        match cmd_shell_test(ShellTestMode::EscDropped) {
            Ok(()) => println!("--- shell test (esc dropped): OK"),
            Err(error) => {
                println!("--- shell test (esc dropped): FAILED ({error})");
                failed.push("shell test (esc dropped)".to_string());
            }
        }

        // **ANSI の解釈の実演（zi-b。ADR-0029）。** 起動シーケンスで CSI を
        // 前景経路へ流し、カーソル位置とセルの中身を判定行で見る。
        // sendkey を使わないので決定的である。
        total += 1;
        begin_item("the console interprets CUP / ED / EL through the foreground path");
        match cmd_ansi_test(&[]) {
            Ok(()) => println!("--- ansi test: OK"),
            Err(error) => {
                println!("--- ansi test: FAILED ({error})");
                failed.push("ansi test".to_string());
            }
        }

        // **破壊の側（zi-b）。** パーサは在るのに前景経路が呼ばない——
        // 接続の取り違えである。CSI がグリフとして化けて出るので、
        // カーソル位置とセルの判定が落ちる。
        total += 1;
        begin_item("the ansi test catches a foreground path that skips the parser");
        match cmd_ansi_test(&["ansi-console-skip-parse-test"]) {
            Ok(()) => {
                println!("--- ansi test (skip parse): FAILED (the sabotage was NOT caught)");
                failed.push("ansi test (skip parse)".to_string());
            }
            Err(_) => println!("--- ansi test (skip parse): OK (the sabotage was caught)"),
        }

        // **SGR の色を渡さない破壊（ES-b。ADR-0040）。** パーサは正しく
        // 展開しており状態も届いているが、**渡す先だけが欠ける**——
        // zi-b の「接続の取り違え」と同じ族である。
        total += 1;
        begin_item("the ansi test catches an SGR that never reaches the color");
        match cmd_ansi_test(&["ansi-sgr-ignore-color-test"]) {
            Ok(()) => {
                println!("--- ansi test (sgr ignored): FAILED (the sabotage was NOT caught)");
                failed.push("ansi test (sgr ignored)".to_string());
            }
            Err(_) => println!("--- ansi test (sgr ignored): OK (the sabotage was caught)"),
        }

        // **DECTCEM の隠す指示を無視する破壊（ES-c）。** 指示は届いて
        // いるが、**描く側が見ない**——「隠した後に無い」判定が落ちる。
        total += 1;
        begin_item("the ansi test catches a cursor that ignores DECTCEM");
        match cmd_ansi_test(&["ansi-cursor-ignore-hide-test"]) {
            Ok(()) => {
                println!(
                    "--- ansi test (cursor ignores hide): FAILED (the sabotage was NOT caught)"
                );
                failed.push("ansi test (cursor ignores hide)".to_string());
            }
            Err(_) => println!("--- ansi test (cursor ignores hide): OK (the sabotage was caught)"),
        }

        // **穴を 0 として読まない破壊（ADR-0038）。** 既定の起動ログが
        // `fs-sparse` の判定行を固定しているので、**破壊は起動ログの差として
        // 出る**——ここでは「その構成で起動が通らないこと」を見る。
        // **`/data/sparse-hole` の読みが落ち、corrupt-fs の期待も食い違う**
        // （実測。2 つの経路で捕まる）。
        total += 1;
        begin_item("refusing holes breaks the sparse read and the corrupt-fs probe");
        match cmd_boot_with_features(&["ext2-sparse-as-error-test"], "fs-sparse", "= true") {
            Ok(()) => {
                println!("--- sparse read (refused): FAILED (the sabotage was NOT caught)");
                failed.push("sparse read (refused)".to_string());
            }
            Err(_) => println!("--- sparse read (refused): OK (the sabotage was caught)"),
        }

        // **`.bss` を張らない破壊（ADR-0039）。** 既定の起動ログが
        // `bss-check` の判定行を固定しているので、**破壊はその行が
        // `Exited(0)` でなくなる形で出る**（実測では `Folded(14)`＝#PF）。
        total += 1;
        begin_item("mapping segments by filesz drops the .bss");
        match cmd_boot_with_features(&["user-load-filesz-only"], "bss-check", "Exited(0)") {
            Ok(()) => {
                println!("--- bss mapping (filesz only): FAILED (the sabotage was NOT caught)");
                failed.push("bss mapping (filesz only)".to_string());
            }
            Err(_) => println!("--- bss mapping (filesz only): OK (the sabotage was caught)"),
        }

        // **`zi` の実演（zi-d）。** 決定的な台本入力で、開いて動いて編集し、
        // `:wq` で保存し、`cat` で読み戻すところまでを見る。
        total += 1;
        begin_item("zi moves, edits, saves, and the file reads back");
        match cmd_zi_test(&[]) {
            Ok(()) => println!("--- zi test: OK"),
            Err(error) => {
                println!("--- zi test: FAILED ({error})");
                failed.push("zi test".to_string());
            }
        }

        // **`less` の実演（VIEW-b）。** **`zi-test` とは別の台本である**
        // ——**あちらは21秒掛かり、破壊19構成すべてに掛かる**ので、
        // **`less` の打鍵を混ぜない**（運用者の承認）。
        //
        // **判定は画面の実物である。** **`less` は内部状態を1つも出さない。**
        total += 1;
        begin_item("less shows a window and gives the screen back; more leaves its output");
        match cmd_view_test(&[]) {
            Ok(()) => println!("--- view test: OK"),
            Err(error) => {
                println!("--- view test: FAILED ({error})");
                failed.push("view test".to_string());
            }
        }

        // **`less` の破壊 1 種。** **窓を動かさない**——**打鍵は届いており、
        // 状態行も描き直されるので、雑に見ると気づけない。**
        // **落ちるのは「窓の外に在った行が見えるようになった」判定だけである。**
        //
        // **`less` の窓を止める形と、`more` が代替画面へ入る形の 2 種である。**
        // **後者は `less` の振る舞いそのもので、`more` との違いを消す**
        // ——**「出したものが残る」判定だけが落ちる。**
        for feature in [
            "less-window-frozen-test",
            "more-uses-alternate-screen-test",
            "flush-every-write-test",
            "read-skip-flush-test",
            "frame-write-per-piece-test",
            "draw-pixel-by-pixel-test",
            "less-redraw-whole-screen-test",
        ] {
            total += 1;
            begin_item(&format!("the view test catches {feature}"));
            match cmd_view_test(&[feature]) {
                Ok(()) => {
                    println!("--- view test ({feature}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("view test ({feature})"));
                }
                Err(_) => println!("--- view test ({feature}): OK (the sabotage was caught)"),
            }
        }

        // **`zi` の破壊 16 種。** 上下を捨てる（zi-d-1）、`:w` が中身を
        // 書かない、挿入が 1 字落とす（どちらも zi-d-2）、プロンプトの色を
        // 送らない、状態行がモードに追随しない（どちらも ES-d）、
        // `ioctl(TIOCGWINSZ)` が行と桁を入れ替える（e-1）、
        // `-EAGAIN` で Esc を確定しない（e-2）、代替画面から戻るときに
        // 描き直さない（e-3）、状態行を本文の下へ置く / コマンド行を描き直さない /
        // `a` を `i` と同じにする（どれも e-4）、`O_CREAT` を受けても作らない（e-5）、
        // **`envp` から `TERM` を落とす（EV）**、
        // **`unlink` を受けても消さない（DIR-1b）。**
        //
        // **落とす判定はそれぞれ違う**——順に、矢印の札の推移 / 往復 /
        // 挿入の本数 / プロンプトの色 / 状態行の札の変化 / 大きさの突き合わせ /
        // 札の並び（ノーマル・インサート・ノーマル）/ 戻った画面の実物 /
        // 状態行の行番号 / 最下行の字 / `a` の桁 / 新しいファイルの有無 /
        // **プロンプトの色** / **消した後の一覧**である。
        //
        // **`env-drop-term` は ES-d の色の判定に乗る（EV）。** **新しい判定器を
        // 足していない**——**画面の実物（バックバッファのピクセル）を読む側が
        // 既に在り、`TERM` が届かなければプロンプトの連なりが見つからない。**
        //
        // **落ちるのは 3 本である**（実測）——色 / 記号 / 代替画面の復帰。
        // **根は 1 つで、どれも色付きの連なりを目印にしている。**
        // **固有に捕まえるのは「`TERM` を読まずに常に色を付ける」形である**
        // ——**既定の構成ではどの判定も落ちないので、この破壊が無ければ
        // 「環境が色を決めている」ことを誰も主張していない。**
        // **`:w` の量の判定はどれでも通る**（要求 0 に対して 0 なので）。
        //
        // **`stderr-on-screen-test` は ADR-0046 の前の振る舞いへ戻す**——
        // **全画面のアプリが動く間も `fd 2` と診断を画面へ書く。**
        // **落ちるのは 2 本である**——**`screen-window`（画面の行 0 が診断行に
        // 化ける）と `screen-echo`（エラーがカーソルの居る行へ出て、
        // エコーエリアが空のままになる）。**
        //
        // **3 つは長い間「別の理由で」落ちていた**——破壊ビルドの像が
        // ディスクへ載らず、シェルが起きる前に停止していた
        // （`docs/troubleshooting.md`。ES-d で直した）。
        for feature in [
            "zi-cursor-ignore-updown-test",
            "zi-write-skip-body-test",
            "zi-insert-drop-first-test",
            "zash-prompt-drop-color-test",
            "zi-status-freeze-mode-test",
            "ioctl-winsize-swap-test",
            "zi-esc-needs-second-key-test",
            "alt-screen-skip-repaint-test",
            "zi-status-below-text-test",
            "zi-command-line-silent-test",
            "zi-append-like-insert-test",
            "open-ignore-create-test",
            "env-drop-term-test",
            "unlink-ignore-request-test",
            "zi-enter-does-nothing-test",
            "brk-skip-shrink-test",
            "zi-skip-release-test",
            "zi-skip-grow-test",
            "zi-join-does-nothing-test",
            "zi-window-frozen-test",
            "stderr-on-screen-test",
            "zi-skip-cursor-flush-test",
            "zi-redraw-whole-screen-test",
            "cursor-repaint-always-test",
            "zi-edit-redraws-everything-test",
            // **P-c-1 で 1 つ増えた。** **据え忘れが黙る形を塞ぐ。**
            "virtio-skip-install-test",
            // **PERF-h で 1 つ増えた。** **出る絵は同じで、費用だけが増える形。**
            "repaint-blank-cells-test",
        ] {
            total += 1;
            begin_item(&format!("the zi test catches {feature}"));
            match cmd_zi_test(&[feature]) {
                Ok(()) => {
                    println!("--- zi test ({feature}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("zi test ({feature})"));
                }
                Err(_) => println!("--- zi test ({feature}): OK (the sabotage was caught)"),
            }
        }

        // **キーボードの配列の切り替え（f-1b。`ADR-0052` の `KEYMAP`）。**
        //
        // **`zi-e` の `keyboard-us-layout-test` を置き換えた**——
        // **実行時に選べるようになったので「US を選ぶ」は正常な経路で、
        // 破壊ではない。** **表そのものの差はホストの単体テストが主張して
        // いる**（`the_two_layouts_differ_where_they_should`）。
        for (label, sabotage) in [("keymap (us)", false), ("keymap (us, always jis)", true)] {
            total += 1;
            begin_item(&format!("the keyboard layout claim: {label}"));
            match cmd_keymap_test(sabotage) {
                Ok(()) => println!("--- {label}: OK"),
                Err(error) => {
                    println!("--- {label}: FAILED ({error})");
                    failed.push(label.to_string());
                }
            }
        }

        // **中断（Ctrl+C）の破壊（S12 前の手当て、C）。**
        //
        // **6 つとも「通らないこと」を期待する**（`ShellTestMode::MustFail`）。
        // **DIR-1 で `env-drop-path-test` が 1 つ加わった。**
        // **落ちる判定は 1 つずつ違う**ので、まとめて 1 項目にはしない——
        // **どれが捕まらなくなったのかが、項目の名前で分かる形にする。**
        // **1 度目に作った変化が 2 度目に見えること（P-a）。**
        //
        // **QEMU を 2 度起こす唯一の項目である。** **間で像を作り直さない。**
        // **判定 7 本を 1 項目にまとめてある**——**どれが落ちても「持ち越せて
        // いない」の 1 つの主張である。**
        total += 1;
        begin_item("a change made in one boot is there in the next");
        match cmd_persist_test(false) {
            Ok(()) => println!("--- persist: OK"),
            Err(error) => {
                println!("--- persist: FAILED ({error})");
                failed.push("persist".to_string());
            }
        }

        // **破壊の側（P-a）。** **2 度目の前に像を作り直す。**
        // **持ち越さない形へ戻すので、持ち越しを主張する 3 本が落ちる**（実測）。
        total += 1;
        begin_item("the persist test catches rebuilding the disk in between");
        match cmd_persist_test(true) {
            Ok(()) => println!("--- persist (rebuilt in between): OK (the sabotage was caught)"),
            Err(error) => {
                println!("--- persist (rebuilt in between): FAILED ({error})");
                failed.push("persist (rebuilt in between)".to_string());
            }
        }

        // **環境の源がファイルであること（f-1。`ADR-0052`）。**
        //
        // **`zi` で `/etc/environment` を書き換え、2 度目の環境が変わる。**
        // **判定 4 本を 1 項目にまとめてある**——**どれが落ちても
        // 「源がファイルになっていない」の 1 つの主張である。**
        for (label, rebuild, ignore) in [
            ("persist (env)", false, false),
            ("persist (env, rebuilt in between)", true, false),
            ("persist (env, the source is ignored)", false, true),
        ] {
            total += 1;
            begin_item(&format!("the environment source claim: {label}"));
            match cmd_persist_env_test(rebuild, ignore) {
                Ok(()) => println!("--- {label}: OK"),
                Err(error) => {
                    println!("--- {label}: FAILED ({error})");
                    failed.push(label.to_string());
                }
            }
        }

        // **`zi` で保存したものが 2 度目に見えること（P-c-3）。**
        //
        // **P-a と変化の作り方が違う**——**あちらはカーネルの中の破壊、
        // こちらは Ring 3 の利用者の操作である。** **判定 6 本を 1 項目に
        // まとめてある**——**どれが落ちても「保存が持ち越せていない」の
        // 1 つの主張である。**
        total += 1;
        begin_item("what zi saved is there in the next boot");
        match cmd_persist_zi_test(false) {
            Ok(()) => println!("--- persist (zi): OK"),
            Err(error) => {
                println!("--- persist (zi): FAILED ({error})");
                failed.push("persist (zi)".to_string());
            }
        }

        // **破壊の側（P-c-3）。** **2 度目の前に像を作り直す。**
        // **2 度目の Ring 3 は建てたままの本文を出すので、突き合わせが落ちる。**
        total += 1;
        begin_item("the zi persist test catches rebuilding the disk in between");
        match cmd_persist_zi_test(true) {
            Ok(()) => {
                println!("--- persist (zi, rebuilt in between): OK (the sabotage was caught)")
            }
            Err(error) => {
                println!("--- persist (zi, rebuilt in between): FAILED ({error})");
                failed.push("persist (zi, rebuilt in between)".to_string());
            }
        }

        // **像を複製して取り出し、建てた像と突き合わせる（S12-a）。**
        // **判定 3 本を 1 項目にまとめてある**（複製先の位置・バイト一致・`e2fsck`）。
        total += 1;
        begin_item("the copied ext2 image comes back byte for byte");
        match cmd_fs_image_extract(&[]) {
            Ok(()) => println!("--- fs extract: OK"),
            Err(error) => {
                println!("--- fs extract: FAILED ({error})");
                failed.push("fs extract".to_string());
            }
        }

        // **破壊の側（S12-a）。** **`e2fsck` では捕まらない**——潰した 1 バイトは
        // 使われていない末尾にあり、あちらは無傷と判定する（実測）。
        // **捕まえるのはバイト一致である。**
        total += 1;
        begin_item("the fs extract catches a corrupted copy");
        match cmd_fs_image_extract(&["fs-copy-corrupt-tail-test"]) {
            Ok(()) => {
                println!("--- fs extract (corrupt tail): FAILED (the sabotage was NOT caught)");
                failed.push("fs extract (corrupt tail)".to_string());
            }
            Err(_) => println!("--- fs extract (corrupt tail): OK (the sabotage was caught)"),
        }

        // **「読む側を複製へ向けたことの反証」は P-e で落とした。**
        // **埋め込み像を外したので、読む先が 1 つしかない**——**あの破壊が
        // 守っていた性質は構造的に真である**（`ADR-0034` の Addendum の引き継ぎの表）。

        // **空き数の欄を正しい位置から読んでいることの反証（S12-b の 2 段目）。**
        // **自分の解析を自分で確かめても、欄を取り違えていれば気づけない。**
        total += 1;
        begin_item("the fs extract catches a shifted group-descriptor field");
        match cmd_fs_image_extract(&["ext2-group-count-offset-test"]) {
            Ok(()) => {
                println!("--- fs extract (shifted field): FAILED (the sabotage was NOT caught)");
                failed.push("fs extract (shifted field)".to_string());
            }
            Err(_) => println!("--- fs extract (shifted field): OK (the sabotage was caught)"),
        }

        // **割り当てと解放（S12-b の 3 段目）。**
        // **判定 1 と判定 2 は像の状態が違うので、同じ起動では両方言えない。**
        // 既定の構成が往復（バイト一致）、`fs-alloc-keep-test` が割り当て中
        // （`e2fsck` の不満が 1 本）である。
        total += 1;
        begin_item("the block bitmap round trip restores the image");
        match cmd_fs_image_extract(&[KEEP_ALLOCATED_FEATURE]) {
            Ok(()) => println!("--- fs bitmap (allocated): OK"),
            Err(error) => {
                println!("--- fs bitmap (allocated): FAILED ({error})");
                failed.push("fs bitmap (allocated)".to_string());
            }
        }

        // **追記（S12-c）。** **書いたままの像でしか判定 A・B・D は言えない。**
        total += 1;
        begin_item("the appended bytes survive a round trip through the image");
        match cmd_fs_image_extract(&[WRITE_KEEP_FEATURE]) {
            Ok(()) => println!("--- fs write (kept): OK"),
            Err(error) => {
                println!("--- fs write (kept): FAILED ({error})");
                failed.push("fs write (kept)".to_string());
            }
        }

        // **縮める道（S12-d）。** **0 まで縮める道は戻さない変種で通る。**
        total += 1;
        begin_item("shrinking a file returns exactly the blocks it should");
        match cmd_fs_image_extract(&[TRUNCATE_KEEP_FEATURE]) {
            Ok(()) => println!("--- fs truncate (emptied): OK"),
            Err(error) => {
                println!("--- fs truncate (emptied): FAILED ({error})");
                failed.push("fs truncate (emptied)".to_string());
            }
        }

        // **作成と削除（S12-e）。** **作ったままの像でしか判定 A・B・D は言えない。**
        total += 1;
        begin_item("a created file survives a round trip through the image");
        match cmd_fs_image_extract(&[CREATE_KEEP_FEATURE]) {
            Ok(()) => println!("--- fs create (kept): OK"),
            Err(error) => {
                println!("--- fs create (kept): FAILED ({error})");
                failed.push("fs create (kept)".to_string());
            }
        }

        for (label, features) in FS_CREATE_SABOTAGES {
            total += 1;
            begin_item(&format!("the fs create check catches {label}"));
            match cmd_fs_image_extract(features) {
                Ok(()) => {
                    println!("--- fs create ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("fs create ({label})"));
                }
                Err(_) => println!("--- fs create ({label}): OK (the sabotage was caught)"),
            }
        }

        // **ディレクトリの作成と削除（DIR-1c）。**
        total += 1;
        begin_item("a created directory survives a round trip through the image");
        match cmd_fs_image_extract(&[MKDIR_KEEP_FEATURE]) {
            Ok(()) => println!("--- fs mkdir (kept): OK"),
            Err(error) => {
                println!("--- fs mkdir (kept): FAILED ({error})");
                failed.push("fs mkdir (kept)".to_string());
            }
        }

        for (label, features) in FS_MKDIR_SABOTAGES {
            total += 1;
            begin_item(&format!("the fs mkdir check catches {label}"));
            match cmd_fs_image_extract(features) {
                Ok(()) => {
                    println!("--- fs mkdir ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("fs mkdir ({label})"));
                }
                Err(_) => println!("--- fs mkdir ({label}): OK (the sabotage was caught)"),
            }
        }

        for (label, features) in FS_TRUNCATE_SABOTAGES {
            total += 1;
            begin_item(&format!("the fs truncate check catches {label}"));
            match cmd_fs_image_extract(features) {
                Ok(()) => {
                    println!("--- fs truncate ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("fs truncate ({label})"));
                }
                Err(_) => println!("--- fs truncate ({label}): OK (the sabotage was caught)"),
            }
        }

        for (label, features) in FS_WRITE_SABOTAGES {
            total += 1;
            begin_item(&format!("the fs write check catches {label}"));
            match cmd_fs_image_extract(features) {
                Ok(()) => {
                    println!("--- fs write ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("fs write ({label})"));
                }
                Err(_) => println!("--- fs write ({label}): OK (the sabotage was caught)"),
            }
        }

        for (label, features) in FS_BITMAP_SABOTAGES {
            total += 1;
            begin_item(&format!("the fs bitmap check catches {label}"));
            match cmd_fs_image_extract(features) {
                Ok(()) => {
                    println!("--- fs bitmap ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("fs bitmap ({label})"));
                }
                Err(_) => println!("--- fs bitmap ({label}): OK (the sabotage was caught)"),
            }
        }

        // **PCI の列挙（S13-a）。** 判定は QEMU 自身の帳簿（`info pci`）との
        // 突き合わせで、期待値の定数を持たない。
        total += 1;
        begin_item("the pci enumeration matches qemu's own device list");
        match cmd_pci_test(&[]) {
            Ok(()) => println!("--- pci enumeration: OK"),
            Err(error) => {
                println!("--- pci enumeration: FAILED ({error})");
                failed.push("pci enumeration".to_string());
            }
        }

        for (label, feature) in PCI_SABOTAGES {
            total += 1;
            begin_item(&format!("the pci enumeration catches {label}"));
            match cmd_pci_test(&[feature]) {
                Ok(()) => {
                    println!("--- pci enumeration ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("pci enumeration ({label})"));
                }
                Err(_) => println!("--- pci enumeration ({label}): OK (the sabotage was caught)"),
            }
        }

        // **virtio-blk の読み（S13-b）。** 判定はホスト側の像のファイルとの
        // 突き合わせで、期待値の定数を持たない。
        total += 1;
        begin_item("the virtio-blk read agrees with the image file");
        match cmd_virtio_test(&[]) {
            Ok(()) => println!("--- virtio blk read: OK"),
            Err(error) => {
                println!("--- virtio blk read: FAILED ({error})");
                failed.push("virtio blk read".to_string());
            }
        }

        for (label, feature) in VIRTIO_SABOTAGES {
            total += 1;
            begin_item(&format!("the virtio-blk read catches {label}"));
            match cmd_virtio_test(&[feature]) {
                Ok(()) => {
                    println!("--- virtio blk read ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("virtio blk read ({label})"));
                }
                Err(_) => println!("--- virtio blk read ({label}): OK (the sabotage was caught)"),
            }
        }

        // **割り込みの配送（S13-d）。** 判定は配線の読み戻し（level と
        // active-low がハードウェアに載っている）と、届いた数である。
        total += 1;
        begin_item("the virtio interrupt arrives as routed");
        match cmd_virtio_irq_test(&[]) {
            Ok(()) => println!("--- virtio irq: OK"),
            Err(error) => {
                println!("--- virtio irq: FAILED ({error})");
                failed.push("virtio irq".to_string());
            }
        }

        total += 1;
        begin_item("the virtio interrupt catches an edge-signaled route");
        match cmd_virtio_irq_test(&["virtio-intx-edge-test"]) {
            Ok(()) => {
                println!("--- virtio irq (edge route): FAILED (the sabotage was NOT caught)");
                failed.push("virtio irq (edge route)".to_string());
            }
            Err(_) => println!("--- virtio irq (edge route): OK (the sabotage was caught)"),
        }

        // **落ち方が 4 形で全部違う**——edge は読み戻し、EOI 落としは 2 回目の
        // 上限つき待ち、ISR 読み落としは数の爆発、BKL 保持待ちは次に BKL を
        // 取る者の再取得検出である（d-2。§6 違反）。**open-wakeup-window は
        // ここに無い**——QEMU の TCG では眠りが起きず、決定的に踏めない
        // （`deferred-decisions.md`）。
        for (label, feature) in [
            ("a dropped EOI", "virtio-skip-eoi-test"),
            ("an unread ISR", "virtio-skip-isr-read-test"),
            ("a wait that holds the BKL", "virtio-wait-holding-bkl-test"),
        ] {
            total += 1;
            begin_item(&format!("the virtio interrupt catches {label}"));
            match cmd_virtio_irq_test(&[feature]) {
                Ok(()) => {
                    println!("--- virtio irq ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("virtio irq ({label})"));
                }
                Err(_) => println!("--- virtio irq ({label}): OK (the sabotage was caught)"),
            }
        }

        // **像のロードの破壊（S13-c）。** どちらも fs extract の判定が捕まえる
        // ——取り違えは blockstats の下限、先頭の欠けはバイト一致である。
        for (label, feature) in FS_LOAD_SABOTAGES {
            total += 1;
            begin_item(&format!("the fs image load catches {label}"));
            match cmd_fs_image_extract(&[feature]) {
                Ok(()) => {
                    println!("--- fs image load ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("fs image load ({label})"));
                }
                Err(_) => println!("--- fs image load ({label}): OK (the sabotage was caught)"),
            }
        }

        // **書き戻し（flush）の破壊（S13-e）。** keep 変種と組む——最終形が
        // 「割り当てたまま」の像で、flush を飛ばすと disk0.img が建てた像の
        // ままになる。**帳簿の下限（wr_bytes）とバイト一致の両方が落ちる。**
        // **落ち方が違う2形**——skip は全部書かず wr_bytes=0、short は先頭
        // 4KiB を欠いて wr_bytes が 4KiB 少なく superblock が食い違う。
        // どちらも keep 変種と組む（最終形が「割り当てたまま」）。
        for (label, features) in [
            (
                "a skipped write-back",
                &["fs-flush-skip-test", KEEP_ALLOCATED_FEATURE][..],
            ),
            (
                "a short write-back",
                &["virtio-flush-short-test", KEEP_ALLOCATED_FEATURE][..],
            ),
        ] {
            total += 1;
            begin_item(&format!("the fs image flush catches {label}"));
            match cmd_fs_image_extract(features) {
                Ok(()) => {
                    println!("--- fs image flush ({label}): FAILED (the sabotage was NOT caught)");
                    failed.push(format!("fs image flush ({label})"));
                }
                Err(_) => println!("--- fs image flush ({label}): OK (the sabotage was caught)"),
            }
        }

        for feature in SHELL_TEST_SABOTAGES {
            total += 1;
            begin_item(&format!("the shell test catches the sabotage {feature}"));
            match cmd_shell_test(ShellTestMode::MustFail(feature)) {
                Ok(()) => println!("--- shell test ({feature}): OK"),
                Err(error) => {
                    println!("--- shell test ({feature}): FAILED ({error})");
                    failed.push(format!("shell test ({feature})"));
                }
            }
        }
    }

    total += 1;
    begin_item("direct serial ports stay on the approved list");
    let mut approved_direct_serial_occurrences = 0usize;
    let unapproved_direct_serial = find_unapproved_direct_serial_ports(
        &workspace_root,
        &mut approved_direct_serial_occurrences,
    )?;
    if unapproved_direct_serial.is_empty() {
        println!(
            "--- direct serial ports: OK ({} approved entr(y/ies) = file+item pairs, covering {} \
             occurrence(s) = SerialPort::new lines)",
            DIRECT_SERIAL_PORT_ALLOWLIST.len(),
            approved_direct_serial_occurrences
        );
    } else {
        for finding in &unapproved_direct_serial {
            println!("    {finding}");
        }
        println!("    approved sites (file / item / reason):");
        for site in DIRECT_SERIAL_PORT_ALLOWLIST {
            println!("      {} / {} / {}", site.file, site.item, site.reason);
        }
        println!(
            "--- direct serial ports: FAILED ({} unapproved site(s); route the line through the \
             logger held inside the BKL, or add an entry with a reason)",
            unapproved_direct_serial.len()
        );
        failed.push("direct serial ports".to_string());
    }

    total += 1;
    begin_item("parsed external tools are called with a fixed locale");
    let direct_tool_calls = find_direct_external_tool_calls(&workspace_root)?;
    if direct_tool_calls.is_empty() {
        println!(
            "--- external tool locale: OK ({} tool(s) go through external_tool(): {})",
            PARSED_EXTERNAL_TOOLS.len(),
            PARSED_EXTERNAL_TOOLS.join(", ")
        );
    } else {
        for finding in &direct_tool_calls {
            println!("    {finding}");
        }
        println!(
            "--- external tool locale: FAILED ({} direct call(s); the output is translated by \
             the environment, and a plausible number from the wrong section reads as success)",
            direct_tool_calls.len()
        );
        failed.push("external tool locale".to_string());
    }

    total += 1;
    begin_item("private-by-design modules keep their internals private");
    let leaks = find_boundary_visibility_leaks(&workspace_root)?;
    if leaks.is_empty() {
        println!(
            "--- private boundaries: OK (no visibility qualifier on mod/use/type under {})",
            PRIVATE_BOUNDARY_DIRS.join(", ")
        );
    } else {
        for finding in &leaks {
            println!("    {finding}");
        }
        println!(
            "--- private boundaries: FAILED ({} export(s); the boundary must be the only way in)",
            leaks.len()
        );
        failed.push("private boundaries".to_string());
    }

    total += 1;
    begin_item("every kernel feature appears in the runtime TEST_HOOKS table");
    let uncovered = find_features_missing_from_test_hooks(&workspace_root)?;
    if uncovered.is_empty() {
        println!(
            "--- test hooks coverage: OK (every feature in kernel/Cargo.toml is either in \
             TEST_HOOKS or on the {}-entry exclusion list)",
            TEST_HOOKS_EXCLUSIONS.len()
        );
    } else {
        for finding in &uncovered {
            println!("    {finding}");
        }
        println!("    exclusions (feature / reason):");
        for (feature, reason) in TEST_HOOKS_EXCLUSIONS {
            println!("      {feature} / {reason}");
        }
        println!(
            "--- test hooks coverage: FAILED ({} feature(s) not reported at boot)",
            uncovered.len()
        );
        failed.push("test hooks coverage".to_string());
    }

    total += 1;
    begin_item("the default kernel build has no sabotage features");
    let sabotage = check_default_features_are_clean(&workspace_root)?;
    if sabotage.is_empty() {
        println!("--- default features: OK");
    } else {
        for finding in &sabotage {
            println!("    {finding}");
        }
        println!("--- default features: FAILED");
        failed.push("default features".to_string());
    }

    total += 1;
    begin_item("every deferred decision carries a state marker");
    match check_deferred_state_markers(&workspace_root) {
        Ok((open, done)) => println!(
            "--- deferred markers: OK ({open} open, {done} settled, {} row(s) total; the count is \
             reported, not enforced)",
            open + done
        ),
        Err(findings) => {
            for finding in &findings {
                println!("    {finding}");
            }
            println!("--- deferred markers: FAILED");
            failed.push("deferred markers".to_string());
        }
    }

    total += 1;
    begin_item("every Bash hook still decides the way it says it does");
    // **hook が読み込まれているかは、ここでは分からない**——**ツールの
    // 呼び出しを止めるのは harness の側で、`xtask` からは観測できない。**
    // **守れるのは「判定そのものが壊れていないこと」だけである。**
    //
    // **実測で 2 回、素通りする形を踏んでいる**（`docs/troubleshooting.md`）——
    // **部分一致が文書の言及まで拒む形と、内側の heredoc が stdin を奪う形である。**
    // **どちらもエラーを出さずに素通りした。**
    //
    // **読み込まれていることは、実際に打って確かめるしかない**
    // （`CLAUDE.md` の規律）。
    //
    // **1 本を名指しせず、`.claude/hooks/` を走査する。** **名指しにすると、
    // 次に足した hook が覆われないまま緑になる**——`--self-test` を持たない
    // hook は、印の行を出さないので落ちる。
    {
        let dir = workspace_root.join(".claude/hooks");
        let mut hooks: Vec<PathBuf> = match fs::read_dir(&dir) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "py"))
                .collect(),
            Err(error) => {
                println!("--- bash hook self-test: FAILED (could not read {dir:?}: {error})");
                failed.push("bash hook self-test".to_string());
                Vec::new()
            }
        };
        hooks.sort();
        let mut findings: Vec<String> = Vec::new();
        if hooks.is_empty() && dir.is_dir() {
            findings.push(format!("no *.py hook under {}", dir.display()));
        }
        for hook in &hooks {
            // stdin を閉じて呼ぶ。**開けたままだと、`--self-test` を持たない
            // hook が本体の側へ落ちて標準入力を待ち、検査ごと止まる。**
            let output = Command::new("python3")
                .arg(hook)
                .arg("--self-test")
                .current_dir(&workspace_root)
                .stdin(Stdio::null())
                .output();
            match output {
                Ok(output) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    // **印の行を要求する。** 成功の終了値だけだと、
                    // `--self-test` を無視した hook が黙って通る。
                    if !text.contains("case(s) decided as expected") {
                        findings.push(format!(
                            "{}: --self-test printed no verdict line",
                            hook.display()
                        ));
                    }
                }
                Ok(output) => findings.push(format!(
                    "{}: --self-test failed ({})",
                    hook.display(),
                    output.status
                )),
                Err(error) => findings.push(format!("{}: could not run ({error})", hook.display())),
            }
        }
        if findings.is_empty() && !hooks.is_empty() {
            println!(
                "--- bash hook self-test: OK ({} hook(s) under .claude/hooks/, each with a \
                 self-test that decides as it says)",
                hooks.len()
            );
        } else if !findings.is_empty() {
            for finding in &findings {
                println!("    {finding}");
            }
            println!(
                "--- bash hook self-test: FAILED ({} hook(s))",
                findings.len()
            );
            failed.push("bash hook self-test".to_string());
        }
    }

    total += 1;
    begin_item("the index and the tracked .claude/ files name each other");
    match check_agent_index_links(&workspace_root) {
        Ok(count) => println!(
            "--- agent index links: OK (both directions; {count} tracked file(s) under .claude/, \
             each named by CLAUDE.md, and every .claude/ path CLAUDE.md names is tracked)"
        ),
        Err(findings) => {
            for finding in &findings {
                println!("    {finding}");
            }
            println!(
                "--- agent index links: FAILED ({} mismatch(es))",
                findings.len()
            );
            failed.push("agent index links".to_string());
        }
    }

    total += 1;
    begin_item("markdown prose style (tracked .md)");
    let prose = check_markdown_prose_style(&workspace_root)?;
    if prose.is_empty() {
        println!("--- markdown prose style: OK");
    } else {
        for finding in &prose {
            println!("    {finding}");
        }
        println!("--- markdown prose style: FAILED ({} line(s))", prose.len());
        failed.push("markdown prose style".to_string());
    }

    total += 1;
    begin_item("the libc pure functions pass their host tests");
    match check_libc_string_tests(&workspace_root) {
        Ok(summary) => println!("--- libc string tests: OK ({summary})"),
        Err(error) => {
            println!("--- libc string tests: FAILED ({error})");
            failed.push("libc string tests".to_string());
        }
    }

    total += 1;
    begin_item("markdown links, anchors and backticked paths resolve (tracked .md)");
    let references = check_markdown_references(&workspace_root)?;
    if references.is_empty() {
        println!("--- markdown references: OK");
    } else {
        for finding in &references {
            println!("    {finding}");
        }
        println!(
            "--- markdown references: FAILED ({} finding(s))",
            references.len()
        );
        failed.push("markdown references".to_string());
    }

    total += 1;
    begin_item("markdown structure (fence pairs and heading levels, tracked .md)");
    let structure = check_markdown_structure(&workspace_root)?;
    if structure.is_empty() {
        println!("--- markdown structure: OK");
    } else {
        for finding in &structure {
            println!("    {finding}");
        }
        println!(
            "--- markdown structure: FAILED ({} finding(s))",
            structure.len()
        );
        failed.push("markdown structure".to_string());
    }

    total += 1;
    begin_item(&format!("commit message style (prefixes and blank line over all history; \
         body length after {COMMIT_BODY_RULE_COMMIT}; Japanese/ASCII gap since {COMMIT_STYLE_SINCE})"));
    let offenders = check_commit_message_style(&workspace_root)?;
    if offenders.is_empty() {
        println!("--- commit style: OK");
    } else {
        for offender in &offenders {
            println!("    {offender}");
        }
        println!("--- commit style: FAILED ({} commit(s))", offenders.len());
        failed.push("commit style".to_string());
    }

    // 二重選択の検出器が既定ビルドに在ること（S4-c-3-2b、静的）。
    total += 1;
    begin_item("the structural guards are present in the default build");
    match check_structural_guard_symbols_present(&workspace_root) {
        Ok(symbol) => println!("--- guard symbols: OK ({symbol})"),
        Err(e) => {
            println!("    {e}");
            println!("--- guard symbols: FAILED");
            failed.push("guard symbols".to_string());
        }
    }

    // トランポリンのバイト単位一致検査（B-2a-5、静的）。既定ビルドの入口 24 バイトが
    // 期待リテラルと一致すること。base 検査なので `--full` でなくても毎回走る。
    total += 1;
    begin_item("trampoline byte match (default build)");
    match cmd_highhalf_trampoline_check(&workspace_root, &[], true) {
        Ok(()) => println!("--- trampoline byte match: OK"),
        Err(e) => {
            println!("    {e}");
            println!("--- trampoline byte match: FAILED");
            failed.push("trampoline byte match".to_string());
        }
    }

    // 像へ入るテキストが ASCII だけであること（2026-08-31、静的）。
    total += 1;
    begin_item("text that goes into the image is ASCII only");
    match check_image_text_is_ascii(&workspace_root) {
        Ok(summary) => println!("--- image text ASCII: OK ({summary})"),
        Err(e) => {
            println!("    {e}");
            println!("--- image text ASCII: FAILED");
            failed.push("image text ASCII".to_string());
        }
    }

    // カーネルが XMM を持たないこと（`ADR-0058` の Decision 5、静的）。
    total += 1;
    begin_item("the kernel never touches FP state (no XMM register in the default build)");
    match check_kernel_has_no_xmm(&workspace_root) {
        Ok(summary) => println!("--- kernel has no XMM: OK ({summary})"),
        Err(e) => {
            println!("    {e}");
            println!("--- kernel has no XMM: FAILED");
            failed.push("kernel has no XMM".to_string());
        }
    }

    // 埋め込む ext2 の像が `e2fsck` を通ること（S10-a、静的）。
    total += 1;
    begin_item("the embedded ext2 image passes e2fsck");
    match check_fs_image_passes_e2fsck(&workspace_root) {
        Ok(summary) => println!("--- fs image e2fsck: OK ({summary})"),
        Err(e) => {
            println!("    {e}");
            println!("--- fs image e2fsck: FAILED");
            failed.push("fs image e2fsck".to_string());
        }
    }

    // 像に「どこで・誰が建てたか」が残っていないこと（2026-09-06、静的）。
    total += 1;
    begin_item("the embedded ext2 image carries no trace of where or who built it");
    match check_image_has_no_build_traces(&workspace_root) {
        Ok(summary) => println!("--- image build traces: OK ({summary})"),
        Err(e) => {
            println!("    {e}");
            println!("--- image build traces: FAILED");
            failed.push("image build traces".to_string());
        }
    }

    let mut retries: Vec<String> = Vec::new();
    if full {
        // QEMU を起動する回帰チェック。1 種類ごとにカーネルをビルドし直して
        // 起動するため重い。既定では走らせない。段階の完了時、`unsafe`・
        // 割り込み・ページテーブル・GDT・IDT に触れた変更のコミット前、
        // ツールチェインやビルド設定を変更したときに走らせる。
        for test in EXCEPTION_TESTS {
            total += 1;
            let name = format!("exception-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_exception_test(test.name)
            });
        }
        for test in CRITICAL_TESTS {
            total += 1;
            let name = format!("critical-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(CRITICAL_TESTS, "critical-test", test.name, None)
            });
        }
        for test in PAGING_TESTS {
            total += 1;
            let name = format!("paging-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(PAGING_TESTS, "paging-test", test.name, None)
            });
        }
        for test in STACK_TESTS {
            total += 1;
            let name = format!("stack-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(STACK_TESTS, "stack-test", test.name, None)
            });
        }
        for test in TASK_TESTS {
            total += 1;
            let name = format!("task-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(TASK_TESTS, "task-test", test.name, None)
            });
        }
        for test in RING3_TESTS {
            total += 1;
            let name = format!("ring3-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(RING3_TESTS, "ring3-test", test.name, None)
            });
        }
        for test in SYSCALL_TESTS {
            total += 1;
            let name = format!("syscall-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(SYSCALL_TESTS, "syscall-test", test.name, None)
            });
        }
        for test in ACPI_TESTS {
            total += 1;
            let name = format!("acpi-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(ACPI_TESTS, "acpi-test", test.name, None)
            });
        }
        for test in ACPI_SMP_TESTS {
            total += 1;
            let name = format!("acpi-smp-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(ACPI_SMP_TESTS, "acpi-smp-test", test.name, Some(2))
            });
        }
        // **BKL の相互排除の証明（S4-b-4）。KVM を要する。**
        total += 1;
        run_regression(
            "bkl-test exclusion-proof",
            &mut failed,
            &mut retries,
            cmd_bkl_exclusion_proof,
        );
        // BKL 待ちのタイムアウト（S4-b-4）。**-smp 2 が要る**（別コアが保持する）。
        for test in BKL_TIMEOUT_TESTS {
            total += 1;
            let name = format!("bkl-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(BKL_TIMEOUT_TESTS, "bkl-test", test.name, Some(2))
            });
        }
        // BKL の破壊（S4-b-2）。
        for test in BKL_TESTS {
            total += 1;
            let name = format!("bkl-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(BKL_TESTS, "bkl-test", test.name, None)
            });
        }
        // **AP のティックのレートをホストの実時間と突き合わせる（S4-a）。**
        // 較正値を BSP と共有するのは仮定なので、**カーネルの外の基準で確かめる。**
        total += 1;
        run_regression(
            "smp-ap-test ap-timer-rate",
            &mut failed,
            &mut retries,
            cmd_ap_timer_rate,
        );
        // per-CPU スロットが足りない構成（S3-b-2a）。覆いの報告の `false` 側を
        // 評価する構成をここで残す。
        for test in ACPI_SMP4_TESTS {
            total += 1;
            let name = format!("acpi-smp-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(ACPI_SMP4_TESTS, "acpi-smp-test", test.name, Some(4))
            });
        }
        for test in APIC_TESTS {
            total += 1;
            let name = format!("apic-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(APIC_TESTS, "apic-test", test.name, None)
            });
        }
        for test in APIC_DECODE_TESTS {
            total += 1;
            let name = format!("apic-decode-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(APIC_DECODE_TESTS, "apic-decode-test", test.name, None)
            });
        }
        for test in INTERRUPT_TESTS {
            if is_excluded_flaky("interrupt-test", test.name) {
                println!(
                    "=== xtask check: interrupt-test {} は確率的なので --full から外してある（cargo xtask flaky）",
                    test.name
                );
                continue;
            }
            total += 1;
            let name = format!("interrupt-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(INTERRUPT_TESTS, "interrupt-test", test.name, None)
            });
        }
        if is_excluded_flaky("interrupt-test", "keyboard") {
            println!(
                "=== xtask check: interrupt-test keyboard は確率的なので --full から外してある（cargo xtask flaky）"
            );
        } else {
            total += 1;
            run_regression(
                "interrupt-test keyboard",
                &mut failed,
                &mut retries,
                cmd_keyboard_test,
            );
        }
        // S2-d-2 の検査と破壊確認。**健全な `rate` を先頭に置いてある**ので、
        // 破壊が意図した経路だけを壊していることまで確かめられる。
        for test in LAPIC_TIMER_TESTS {
            total += 1;
            let name = format!("lapic-timer-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_lapic_timer_test(test.name)
            });
        }
        // S3-b-2b-2 の sentinel の破壊確認。
        for test in SMP_AP_TESTS {
            if is_excluded_flaky("smp-ap-test", test.name) {
                println!(
                    "=== xtask check: smp-ap-test {} は確率的なので --full から外してある（cargo xtask flaky）",
                    test.name
                );
                continue;
            }
            total += 1;
            let name = format!("smp-ap-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(SMP_AP_TESTS, "smp-ap-test", test.name, Some(2))
            });
        }
        // S3-b-2b-1 の雛形一致検査の破壊確認。
        for test in SMP_TRAMP_TESTS {
            total += 1;
            let name = format!("smp-tramp-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(SMP_TRAMP_TESTS, "smp-tramp-test", test.name, Some(2))
            });
        }
        // S3-b-2a の tripwire の破壊確認。
        for test in PERCPU_TESTS {
            total += 1;
            let name = format!("percpu-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(PERCPU_TESTS, "percpu-test", test.name, None)
            });
        }
        // S2-d-1c の破壊確認。**落ちるべき主張だけが落ちること**を見る。
        // 健全な側も並べて指定しているので、破壊が意図した経路だけを
        // 壊していることまで確かめられる。
        for sabotage in IOAPIC_SABOTAGE_TESTS {
            total += 1;
            let name = format!("ioapic-test {}", sabotage.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_ioapic_sabotage(sabotage.name, sabotage.feature, sabotage.expected)
            });
        }
        total += 1;
        run_regression("panic-test", &mut failed, &mut retries, || {
            cmd_run(&RunOptions {
                panic_test: true,
                keep_disk: false,
                rebuild_disk: false,
                gui: false,
                gtk: false,
                gfx_test: false,
                kvm: false,
                no_limit: false,
                manual: false,
                key_probe: false,
            })
        });
        // higher-half の破壊確認（B-2a-5）。(a)(b)(c) は QEMU で位置署名 + 定常未到達を
        // 判定、(d) はビルド + トランポリンのバイト不一致を静的に判定。
        for test in HIGHHALF_TESTS {
            total += 1;
            let name = format!("highhalf-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_highhalf_test(test.name)
            });
        }
        total += 1;
        run_regression(
            "highhalf-test trampoline-absolute-ref",
            &mut failed,
            &mut retries,
            || {
                cmd_highhalf_trampoline_check(
                    &workspace_root,
                    &["highhalf-trampoline-absolute-ref"],
                    false,
                )
            },
        );
    }

    println!();
    if !retries.is_empty() {
        println!(
            "xtask check: {} check(s) were retried because the target did not start: {}",
            retries.len(),
            retries.join(", ")
        );
    }
    // **項目数が会計行と一致すること。** 検査を足して会計行を更新し忘れる形を
    // 構造で止める（`EXPECTED_CHECK_COUNT` の doc）。**`total` はここで確定して
    // いるので、`cmd_check` の組み替えは要らない。**
    // **最後の項目の所要を出す**（[`begin_item`] の doc。**次の見出しが
    // 前の項目の終わりなので、最後だけはここで締める**）。
    finish_item();
    check_count_matches_accounting(&workspace_root, total, full, commit)?;

    if failed.is_empty() {
        println!("xtask check: all {total} check(s) passed");
        return Ok(());
    }
    bail!(
        "xtask check: {} of {total} check(s) failed: {}",
        failed.len(),
        failed.join(", ")
    );
}

/// 会計行に記録されている項目数（`docs/verification-coverage.md` の「項目会計」）。
///
/// # なぜ二重に持つのか。**片方が機械で強制されるなら、両方が腐るのとは違う**
///
/// 会計行は「検査を足したとき数が閉じていることを、この行だけで追う」場所である。
/// **ところがその行自体が 2 段ぶん古くなっていた**（S2-d-1c の 3 種と S2-d-2 の
/// 4 種を足したときに更新しておらず、`--full` が 77 のまま残っていた）。
/// **単一の出所と決めた場所が古くなると、他のすべての参照が正しくても会計は止まる。**
///
/// 以前この案を「二重持ちの場所が変わるだけ」として見送った記録があるが、
/// **weigh きれていなかった点がある。** この定数は**合わないとビルド（検査）が
/// 落ちる**。docs の 2 箇所が両方とも静かに腐るのとは性質が違う。
///
/// # **強制されるのは定数の更新だけである。会計行の更新は強制されない**
///
/// 検査を足すとここが落ち、直すには定数を上げる必要がある。**そのとき この doc が
/// 会計行を指しているので、「検査を足したら会計行を見る」が手順ではなく構造から
/// 促される。** ただし**促されるだけで、強制ではない。** 定数だけ上げて会計行を
/// 放置することはできる。**半分だけ構造へ移った状態である。** 残りの半分は
/// 依然として規律なので、そう書いておく（守れない箇所を守れると書かない）。
///
/// # 会計行を機械で読む案は、2 度目が起きたので採った
///
/// **以前はこの案を採らなかった。** 理由は「xtask が会計行の書式へ結合し、
/// 文書の書き方が検査の都合で固定される。会計行は人が読んで経緯を辿るための
/// 散文なので釣り合わない」だった。**その判断のあとで、同じ規律が 2 度目に破れた**
/// （S3-b-2b-2 が定数を 88 から 89 へ上げ、会計行は 88 のまま残った）。
///
/// **天秤の重みが変わった。** 促されるだけでは足りないことが、同じ行で 2 度
/// 示された。そこで [`check_accounting_line_lists_current_counts`] を足した。
/// **結合を最小にしてある**——見るのは「`推移: ` で始まる行に、現在の base と
/// full が太字の数字として現れること」だけで、推移の書き方・順序・理由の文言には
/// 触れない。歴史の数字は残るので、**行を消さない**運用とも噛み合う。
///
/// # **強制できるのは数字の鮮度だけである。経緯の正しさは強制されない**
///
/// この検査は `**89**` という文字列が行に在ることしか見ない。**理由を書かずに
/// 数字だけ足せば通る。** 会計行の価値は「どの段が何を足したか」の側にあり、
/// そこは依然として規律である。**守れない箇所を守れると書かないために明示する。**
/// 検査が保証するのは「数が変わったときに、この行が触られること」までである。
///
/// # 走らせる前に総数を出す必要は無い
///
/// 以前の記録は「走らせずに総数を出せるよう `cmd_check` を組み替えるのが前提」と
/// 書いていたが、**照合するだけなら要らない。** `total` は `cmd_check` の末尾で
/// 既に確定している（`all {total} check(s) passed` がそれを印字している）。
/// 組み替えが要るのは「走らせる前に印字する」形の場合だけである。
struct ExpectedCheckCount {
    /// `cargo xtask check`（QEMU を起動しない検査だけ）。
    base: usize,
    /// `cargo xtask check --full`。
    full: usize,
}

/// 会計行の現在値。**検査を足したらここを上げ、あわせて会計行も更新すること。**
const EXPECTED_CHECK_COUNT: ExpectedCheckCount = ExpectedCheckCount {
    base: 32,
    full: 291,
};

/// 実際に走った項目数が会計行と一致するかを見る。
///
/// 一致しないときは**検査の失敗として扱う。** 項目を足したのに会計行を更新して
/// いない状態でコミットへ進めないようにするためである。
fn check_count_matches_accounting(
    workspace_root: &Path,
    total: usize,
    full: bool,
    commit: bool,
) -> Result<()> {
    // `--commit` の期待値は base + 1（boot log diff の 1 項目）で導出する。
    // 定数を持たない——持つと base の検査を足すたびに 2 箇所を直す作業が
    // 生まれ、片方だけが更新される形で腐る。
    let expected = if full {
        EXPECTED_CHECK_COUNT.full
    } else if commit {
        EXPECTED_CHECK_COUNT.base + 1
    } else {
        EXPECTED_CHECK_COUNT.base
    };
    if total != expected {
        let field = if full {
            "full"
        } else if commit {
            "base + 1 (--commit)"
        } else {
            "base"
        };
        bail!(
            "xtask check: ran {total} check(s) but EXPECTED_CHECK_COUNT.{field} is {expected}. \
             If you added or removed a check, update that constant in xtask AND the \
             item-accounting line in docs/verification-coverage.md"
        );
    }
    check_accounting_line_lists_current_counts(workspace_root)
}

/// `TEST_HOOKS` に載せなくてよい feature と、その理由。
///
/// **除外は列挙だが、向きが逆である。** 「列挙に無い形を静かに通す」形ではなく、
/// **列挙に無い feature は TEST_HOOKS に在るはず**という向きに書いてある。
/// 新しい feature を足して両方に入れ忘れれば、静かに通らず落ちる。
const TEST_HOOKS_EXCLUSIONS: &[(&str, &str)] = &[
    ("default", "feature の既定値であって、壊す経路ではない"),
    (
        "heap-poison",
        "解放したメモリを毒値で埋める開発時の補助。壊す経路ではない",
    ),
    (
        "keyboard-raw-log",
        "スキャンコードを生のままログへ出すだけ。壊す経路ではない",
    ),
];

/// `kernel/Cargo.toml` の feature のうち、`TEST_HOOKS` にも除外リストにも
/// 無いものを挙げる。
///
/// # なぜ数ではなく名前の集合で見るのか
///
/// **数を数えるだけの検査は、足した数と消した数が釣り合うと素通りする。**
/// 名前の集合の一致なら、入れ替わりも捕まる。**会計行の強制（数字が在るだけで
/// 満たせる）より強い保証である**（`ExpectedCheckCount` の doc に、あちらで
/// 強制できるのが数字の鮮度だけであることを書いてある）。
///
/// # 3 度目である
///
/// 「一覧が足したときに更新されず静かに狭くなる」は、会計行・
/// `verification-coverage.md` の破壊 feature 一覧に続いて 3 件目である。
/// **今回は機械で守れる形なので、規律に戻さない。**
///
/// # 逆向きは別の機構が守っている。**両向きが揃っている**
///
/// この関数が見るのは片側だけである（Cargo.toml の feature → 表に在ること）。
/// **逆向き、すなわち表に在るのに Cargo.toml に無い feature は、ここでは
/// 捕まらない。**
///
/// **その逆向きは `unexpected_cfgs` lint が構造的に覆っている**（実測）。
/// 存在しない feature 名で `cfg!(feature = "…")` を書くと、`-D warnings` の
/// clippy が落ちる。
///
///     error: unexpected `cfg` condition value: `zzz-not-a-real-feature`
///          = note: `-D unexpected-cfgs` implied by `-D warnings`
///
/// `cargo xtask check` は 4 構成すべてに `-D warnings` を掛けているので、
/// **この経路は既に検査に入っている。**
///
/// **したがって両向きが、別々の機構で守られている。** 片側はこの関数、
/// 逆側は lint である。**逆向きの検査を足さない理由はこれである**（同じことを
/// 言う検査を 2 つ置かない）。**lint を緩める変更（`allow(unexpected_cfgs)` を
/// 足す、`-D warnings` を外す）は、この保証を落とす。**
fn find_features_missing_from_test_hooks(workspace_root: &Path) -> Result<Vec<String>> {
    let manifest = workspace_root.join("kernel").join("Cargo.toml");
    let manifest_text = fs::read_to_string(&manifest)
        .with_context(|| format!("could not read {}", manifest.display()))?;
    let source = workspace_root.join("kernel").join("src").join("main.rs");
    let source_text = fs::read_to_string(&source)
        .with_context(|| format!("could not read {}", source.display()))?;

    let Some(start) = source_text.find(TEST_HOOKS_TABLE_MARKER) else {
        bail!(
            "xtask check: could not find {TEST_HOOKS_TABLE_MARKER:?} in kernel/src/main.rs. \
             If the table was renamed, update TEST_HOOKS_TABLE_MARKER in xtask along with it"
        );
    };
    let table = &source_text[start..];
    let table = match table.find("\n];") {
        Some(end) => &table[..end],
        None => table,
    };

    let mut findings = Vec::new();
    for line in manifest_text.lines() {
        // `feature-name = [...]` の形だけを feature とみなす。
        let Some((name, rest)) = line.split_once(" = ") else {
            continue;
        };
        if !rest.starts_with('[') || name.is_empty() {
            continue;
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            continue;
        }
        if TEST_HOOKS_EXCLUSIONS.iter().any(|(f, _)| *f == name) {
            continue;
        }
        // **`cfg!` の形で探す。** 表示名だけを見ると、名前と `cfg!` が食い違って
        // いても通ってしまう（別の feature の状態を、この名前で報告する形）。
        let needle = format!("cfg!(feature = \"{name}\")");
        if !table.contains(&needle) {
            findings.push(format!(
                "kernel/Cargo.toml: feature {name:?} is not reported by TEST_HOOKS \
                 (add an entry, or put it on the exclusion list with a reason)"
            ));
        }
    }
    Ok(findings)
}

/// `TEST_HOOKS` の表を探す目印。**この形に結合しているのはここだけである。**
const TEST_HOOKS_TABLE_MARKER: &str = "const TEST_HOOKS: &[(&str, bool, &str)] = &[";

/// 会計行がある文書。
const ACCOUNTING_DOC_PATH: &str = "docs/verification-coverage.md";

/// 持ち越しの一覧が状態の印を持っているかを見る（S12 前の手当ての締め）。
///
/// **数は返すが、検査しない。** 持ち越しは増減するのが正常なので、
/// **数そのものを固定すると、行を 1 つ足すたびに定数を直す作業が生まれる。**
/// `EXPECTED_CHECK_COUNT` と同じ族にしないのはそのためである。
///
/// **見るのは 2 つだけである。**
///
/// - 状態の列を持つ表の行が、すべて印を持っていること
/// - その印が語彙（`未` / `済`）の中にあること
///
/// **「印が無い」を静かに通すと、数える側が黙って狭くなる**——
/// `TEST_HOOKS` の doc が「一覧が足したときに更新されず静かに狭くなる」を
/// 3 件目として記録しているのと同じ形である。
///
/// 返すのは `(未の数, 済の数)`。**規則の本体は
/// `docs/deferred-decisions.md` の「持ち越しの数え方」にある。**
/// 索引が指している `.claude/…` のパスを、重複を落として拾う。
///
/// **ディレクトリの言及（`/` で終わるもの）は拾わない。**
/// 「`.claude/` の整備」のような書き方を、指し先として数えないためである。
fn agent_paths_named_in(text: &str) -> Vec<String> {
    const PREFIX: &str = ".claude/";
    let mut found: Vec<String> = Vec::new();
    for (start, _) in text.match_indices(PREFIX) {
        let rest = &text[start..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
            .unwrap_or(rest.len());
        let path = &rest[..end];
        if path.ends_with('/') {
            continue;
        }
        if !found.iter().any(|p| p == path) {
            found.push(path.to_string());
        }
    }
    found
}

/// 索引（`CLAUDE.md`）と、追跡下の `.claude/` のファイルの対応を**両向きに**見る。
///
/// # なぜ要るか
///
/// **`CLAUDE.md` は索引になった。** 節の見出しを残し、本体の在り処を 1 行で指す。
/// **指す先は `.claude/` の中で、そこは今後も分割・改名される。**
/// **切れても機械は止めない**——参照はリンクではないので、`tools/docstyle.py` の
/// `S5`（リンクとアンカーの生存）は見ない。**誰も気づかないまま索引が嘘になる。**
///
/// # 見るのは両向きである
///
/// - **順**: 索引が名前を出した `.claude/…` が、すべて追跡下に在ること。
///   **無いものを指した索引は、その場で嘘である**（公開文書は、リポジトリに
///   無いファイルを指してはならない。`CLAUDE.md` の「エージェント向け設定
///   ファイルの扱い」）。
/// - **逆**: 追跡下の `.claude/…` が、すべて索引から指されていること。
///   **足したのに索引へ載せなければ、次のセッションはその存在を知れない**
///   （`ADR-0031` が追跡下へ入れた理由がこれである）。
///
/// **追跡下かどうかで入力を決めるので、追跡外のものは両向きとも対象にならない。**
/// `.claude/settings.local.json` が逆向きで落ちないのはそのためである
/// （公開しないと決めてある。`ADR-0031`）。
fn find_agent_index_mismatches(index_text: &str, tracked: &[String]) -> Vec<String> {
    let named = agent_paths_named_in(index_text);
    let mut findings = Vec::new();
    for path in &named {
        if !tracked.iter().any(|t| t == path) {
            findings.push(format!(
                "CLAUDE.md names {path}, which is not tracked. The index must not point at a \
                 file the repository does not have"
            ));
        }
    }
    for path in tracked {
        if !named.iter().any(|n| n == path) {
            findings.push(format!(
                "{path} is tracked but CLAUDE.md never names it. Put the pointer in the index, \
                 or the next session cannot find the file"
            ));
        }
    }
    findings
}

/// 上の判定へ入力を集める。**追跡下の一覧は `git ls-files` から取る。**
fn check_agent_index_links(workspace_root: &Path) -> Result<usize, Vec<String>> {
    let index = workspace_root.join("CLAUDE.md");
    let text = match fs::read_to_string(&index) {
        Ok(text) => text,
        Err(e) => return Err(vec![format!("could not read {}: {e}", index.display())]),
    };
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["ls-files", ".claude"])
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return Err(vec![format!(
                "git ls-files .claude failed ({})",
                output.status
            )]);
        }
        Err(e) => return Err(vec![format!("could not run git ls-files: {e}")]),
    };
    let tracked: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    let findings = find_agent_index_mismatches(&text, &tracked);
    if findings.is_empty() {
        Ok(tracked.len())
    } else {
        Err(findings)
    }
}

fn check_deferred_state_markers(workspace_root: &Path) -> Result<(usize, usize), Vec<String>> {
    /// 状態の語彙。**増やすときは doc の「持ち越しの数え方」も直すこと。**
    const VOCABULARY: [&str; 2] = ["未", "済"];

    let path = workspace_root.join("docs/deferred-decisions.md");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return Err(vec![format!("could not read {}: {e}", path.display())]),
    };

    let mut findings = Vec::new();
    let (mut open, mut done) = (0usize, 0usize);
    // 状態の列を持つ表の中にいるか。見出しで入り、表が切れたら出る。
    let mut in_marked_table = false;

    for (number, line) in text.lines().enumerate() {
        let line_number = number + 1;
        if !line.starts_with('|') {
            in_marked_table = false;
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        // 見出し行。**項目の表だけを対象にする。**
        //
        // **列の数も見る。** 「持ち越しの数え方」の語彙の表も見出しが `状態` で
        // 始まるが、2 列しかない。**列を見ないと、語彙の説明そのものが
        // 項目として数えられる**（実測でそうなった。51 と出て、実数より 2 多かった）。
        if cells.first() == Some(&"状態") {
            in_marked_table = cells.len() >= 4;
            continue;
        }
        // 区切り行。
        if line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
            continue;
        }
        if !in_marked_table {
            continue;
        }
        match cells.first() {
            Some(marker) if VOCABULARY.contains(marker) => {
                if *marker == "未" {
                    open += 1;
                } else {
                    done += 1;
                }
            }
            Some(other) => findings.push(format!(
                "docs/deferred-decisions.md:{line_number}: the state cell is {other:?}, which is \
                 not one of {VOCABULARY:?}. The vocabulary is deliberately two words; see \
                 the section 持ち越しの数え方"
            )),
            None => findings.push(format!(
                "docs/deferred-decisions.md:{line_number}: this row has no state cell"
            )),
        }
    }

    if findings.is_empty() {
        Ok((open, done))
    } else {
        Err(findings)
    }
}

/// 会計行の目印。**この行の書式に結合しているのはここだけである。**
const ACCOUNTING_LINE_PREFIX: &str = "推移: ";

/// 会計行が現在の項目数を載せているかを見る。
///
/// **見るのは数字が在ることだけである。** 推移の書き方・順序・理由の文言には
/// 触れない（[`ExpectedCheckCount`] の doc に、何が強制され何が強制されないかを
/// 書いてある）。**歴史の数字は残ってよい**ので、含むことだけを条件にしてある。
fn check_accounting_line_lists_current_counts(workspace_root: &Path) -> Result<()> {
    let path = workspace_root.join(ACCOUNTING_DOC_PATH);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("could not read the accounting doc {}", path.display()))?;

    let Some(line) = text
        .lines()
        .find(|line| line.starts_with(ACCOUNTING_LINE_PREFIX))
    else {
        bail!(
            "xtask check: {ACCOUNTING_DOC_PATH} has no line starting with \
             {ACCOUNTING_LINE_PREFIX:?}. That line is the item-accounting record. If it was \
             renamed, update ACCOUNTING_LINE_PREFIX in xtask along with it"
        );
    };

    for (field, value) in [
        ("base", EXPECTED_CHECK_COUNT.base),
        ("full", EXPECTED_CHECK_COUNT.full),
    ] {
        let needle = format!("**{value}**");
        if !line.contains(&needle) {
            bail!(
                "xtask check: EXPECTED_CHECK_COUNT.{field} is {value}, but the item-accounting \
                 line in {ACCOUNTING_DOC_PATH} does not carry {needle} . Append the new count to \
                 that line, with the stage that added the check(s). Only the digits are enforced; \
                 the reason next to them is not, and it is the part worth having"
            );
        }
    }
    Ok(())
}

/// 起動失敗（環境要因）を表すメッセージの目印。
///
/// [`report_did_not_start`] が返すエラーだけがこれを含む。テストの失敗
/// （実装の問題）とは区別されている。
const DID_NOT_START_MARKER: &str = "kernel did not start (environment, not the code)";

/// 回帰チェックを 1 件走らせ、失敗しても止めずに記録する。
///
/// `--full` は「何が壊れているか」を一度に知るためのものなので、
/// 最初の失敗で打ち切らない。
///
/// # 環境要因のときだけ 1 回だけ再試行する
///
/// OVMF はまれに起動に失敗し、シェル / アイドルループへ落ちる。これは
/// 実装の問題ではないので、そのたびに手で再実行するのは無駄である。
/// xtask が起動失敗と判定した場合に限り、自動で 1 回だけやり直す。
///
/// **再試行したことは必ず出力する。** 黙って通すと、環境が悪化して
/// 起動失敗が常態化しても気づけない。回数を数えて最後にまとめて出す。
///
/// テストの失敗（実装の問題）では再試行しない。落ちるものは落ちたまま
/// 報告する。
/// `--full` から外した確率的な項目（S9-b の途中で、独立した作業として外した）。
///
/// # なぜ外すか
///
/// **確率的なものを `--full` へ入れると、落ちたときに退行か揺らぎかが区別できない。**
/// 方針は `coding-standards.md` にあり、`kernel-entry-concurrency` を外した前例も
/// ある。**同じ性質の 4 項目が入ったままだった**（`deferred-decisions.md` の
/// 持ち越し 12「決めた方針が、後から該当した項目へ適用されていない」）。
///
/// **実測**——T2-c で `--full` 15 回中 4 回（約 27%）、S9-b-2 で 2 回中 2 回。
/// **毎回、落ちたのが退行か揺らぎかを手で切り分けることになる。**
///
/// # 外して何が失われるか
///
/// **前例とは事情が違う。** `kernel-entry-concurrency` は**補助実証**で、
/// `verification-coverage.md` が「何かの主張を担っていない」と書いている。
/// **こちらの 4 つは主張を担っている**（TLB シュートダウン、キーボードの配送、
/// 二層の守り）。**外すと、`cargo xtask flaky` を回さない限り誰も見ない。**
///
/// **そして前例が実際に手で回された記録は無い。** 外したのは S4-a で、
/// それ以降 `kernel-entry-concurrency` を回した記録が docs に見当たらない。
/// **したがって「手で回す」は、手順を置いても回される保証が無い。**
/// **失われるものを正確に書いたうえで外している。**
const FLAKY_EXCLUDED: &[(&str, &str)] = &[
    ("interrupt-test", "keyboard"),
    ("smp-ap-test", "tlb-shootdown"),
    ("smp-ap-test", "smp-stimulus-layer1-off"),
    ("smp-ap-test", "smp-stimulus-both-layers-off"),
];

/// その項目が [`FLAKY_EXCLUDED`] に載っているか。
fn is_excluded_flaky(group: &str, name: &str) -> bool {
    FLAKY_EXCLUDED
        .iter()
        .any(|(g, n)| *g == group && *n == name)
}

/// [`FLAKY_EXCLUDED`] の各行が実在の項目を指しているかを確かめる。
///
/// **列挙で守るものは、列挙が実態からずれると静かに効かなくなる。**
/// 名前を打ち間違えると「外したつもりで外れていない」か「存在しない項目を
/// 外している」になる。どちらも出力からは分からないので、ここで落とす。
fn check_flaky_list_matches_tables() -> Result<()> {
    for (group, name) in FLAKY_EXCLUDED {
        let found = match (*group, *name) {
            // `keyboard` は表の項目ではなく単独の関数である（`cmd_keyboard_test`）。
            // **表だけを見ていると「無い」と判定するので、ここで明示的に扱う。**
            // 実際、最初に表だけを見る形で書いて、この検査に落とされた。
            ("interrupt-test", "keyboard") => true,
            ("interrupt-test", n) => INTERRUPT_TESTS.iter().any(|t| t.name == n),
            ("smp-ap-test", n) => SMP_AP_TESTS.iter().any(|t| t.name == n),
            (other, _) => anyhow::bail!("FLAKY_EXCLUDED names an unknown group {other:?}"),
        };
        if !found {
            anyhow::bail!("FLAKY_EXCLUDED names {group} {name:?}, which no table has");
        }
    }
    Ok(())
}

/// `--full` から外した確率的な項目を手で回す（`cargo xtask flaky`）。
///
/// **外した項目を回す手段がなければ、外すことは「守らないと決める」ことになる。**
/// 1 項目につき最大 [`FLAKY_ATTEMPTS`] 回まで試し、**何回目で通ったかを出す。**
///
/// **回数そのものが情報である。** 1 回目で通り続けているうちは揺らぎが小さく、
/// 3 回目まで要るようになったなら確率が上がっている。**通らなければ退行である。**
fn cmd_flaky() -> Result<()> {
    let mut never_passed: Vec<String> = Vec::new();

    for (group, name) in FLAKY_EXCLUDED {
        let label = format!("{group} {name}");
        let mut passed_on = None;
        for attempt in 1..=FLAKY_ATTEMPTS {
            let result = match (*group, *name) {
                ("interrupt-test", "keyboard") => cmd_keyboard_test(),
                ("interrupt-test", n) => cmd_marker_test(INTERRUPT_TESTS, group, n, None),
                ("smp-ap-test", n) => cmd_marker_test(SMP_AP_TESTS, group, n, Some(2)),
                (other, _) => anyhow::bail!("FLAKY_EXCLUDED names an unknown group {other:?}"),
            };
            if result.is_ok() {
                passed_on = Some(attempt);
                break;
            }
            println!("--- flaky: {label} did not pass on attempt {attempt}");
        }
        match passed_on {
            Some(attempt) => println!("--- flaky: {label}: OK (passed on attempt {attempt})"),
            None => {
                println!("--- flaky: {label}: FAILED ({FLAKY_ATTEMPTS} attempt(s), none passed)");
                never_passed.push(label);
            }
        }
    }

    if never_passed.is_empty() {
        println!(
            "flaky: all {} excluded item(s) passed within {FLAKY_ATTEMPTS} attempt(s)",
            FLAKY_EXCLUDED.len()
        );
        Ok(())
    } else {
        anyhow::bail!(
            "flaky: {} item(s) never passed: {}",
            never_passed.len(),
            never_passed.join(", ")
        )
    }
}

/// [`cmd_flaky`] が 1 項目に許す試行回数。
///
/// **27% の揺らぎなら 3 回で約 2%、5 回で約 0.14% まで落ちる。**
/// 5 回とも落ちたなら揺らぎでは説明しにくく、退行を疑う根拠になる。
/// **ただし「5 回の緑では足りない」は逆向きにも効く**——通ったことは
/// 「揺らぎが消えた」の証拠にはならない（`coding-standards.md`）。
const FLAKY_ATTEMPTS: usize = 5;

/// 落ちた項目の一覧（VIEW-c の後）。
///
/// # なぜ `Vec` を包むのか
///
/// **`--full` が上限で切れたとき、「切れた」と「落ちた」を分けて言うために、
/// 走っている最中の失敗の数が要る**（[`begin_item`] が締めの行に出す）。
/// **`push` を包めば、55 箇所の呼び出し側は 1 文字も変わらない。**
#[derive(Default)]
struct Failures {
    list: Vec<String>,
}

impl Failures {
    fn push(&mut self, name: String) {
        FAILED_SO_FAR.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.list.push(name);
    }

    fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    fn len(&self) -> usize {
        self.list.len()
    }

    fn join(&self, separator: &str) -> String {
        self.list.join(separator)
    }
}

/// ここまでに落ちた項目の数（VIEW-c の後）。
static FAILED_SO_FAR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// ここまでに走った項目の数（VIEW-c の後）。
static ITEMS_DONE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `--full` の自前の上限（VIEW-c の後）。**過ぎたらそこで止める。**
static TIME_LIMIT: std::sync::Mutex<Option<(Instant, std::time::Duration)>> =
    std::sync::Mutex::new(None);

/// `--full` の上限（VIEW-c の後）。
///
/// # なぜ自前で持つのか
///
/// **外側の `timeout` に切られると、締めの行が出ない**——**ログの末尾は
/// QEMU の終了メッセージで終わり、次に見る者は「落ちた」と読む**
/// （実測で 1 度そうなった）。**自分で止めれば「上限で切れた。ここまでは
/// 緑」と書ける。**
///
/// # 値の根拠
///
/// **実測でいちばん遅い回が約52分だった**（3 回走らせて、50分の上限で切れた回が
/// 約97%まで進んでいた／42.4分／42.0分）。**項目の外にも0.9分掛かる。**
/// **90分はその倍近くで、揺れても届かない。**
///
/// # 値を見直すときの実測（SE 段の後。2026-08-28）
///
/// **項目時間の合計は 39.8分（247項目）／50.3分（248項目）／57.4分（249項目）だった。**
/// **いちばん遅い回は52分ではなく57.4分になっている。**
///
/// **差をそのまま「増えた分」と読まないこと。** **項目の数も中身も違い、
/// `--full` の合計はホストの負荷で揺れる**（上の3回も揺れの幅に入る）。
/// **制御して測れたのは `--shell-test` の増分だけである**——**同一セッションで
/// 前後を測り、69.7秒から86.7秒（+17.0秒）になった**（`LINE_MAX` ちょうどの行を
/// 打つ128打鍵ぶんである）。**`--full` はそれを14回走らせるので +4.0分。**
///
/// **90分にはまだ余裕がある。** **次に見直す者は、52分ではなく57.4分から見積もること。**
///
/// # 90分から120分へ上げた（2026-09-01。`ADR-0054` の後）
///
/// **上げた理由は 2 つある。**
///
/// **1 つは実測である**——**261項目で約75分だった**（f-2 の締め。**内訳は
/// `docs/verification-coverage.md` に在る**）。**264項目で 90分に触れた**
/// （**131項目まで進んで切れた**）。**余裕が 15分では、台本を 1 本伸ばすたびに
/// 触ることになる**——**`--shell-test` は 13 構成、`zi-test` は 22 構成に掛かる。**
///
/// **もう 1 つは、切れた回から得られるものが少ないことである。**
/// **上限で切れると、走った項目の合否しか残らない**——**「落ちた21件」が
/// 実装のせいか、機械の混みようかを切り分ける材料が無い。** **長く走らせて
/// 全部の合否を得るほうが、判定として役に立つ。**
///
/// **上限そのものは残す。** **止まった `--full` を何時間も放置しないための
/// ものである**（この doc の上の理由）。**120分は、いちばん遅い実測（75分）の
/// 1.6倍である。**
const FULL_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(120 * 60);

/// 項目の所要を測る時計（VIEW-b の後）。
///
/// # なぜ在るのか
///
/// **`--full` の所要が説明できなくなった**——**足した項目では説明の付かない
/// 差が出た**（実測。**42分と、上限で切れた50分超**）。**内訳が出ないと、
/// 次に伸びたときに「切れたのか壊れたのか」も、何を疑うかも決まらない。**
///
/// # 判定行には載せない
///
/// **時間は揺れる。** **同じ構成の同じ `--full` が42分と50分超になった**
/// （実測）。**揺れる値を判定行に載せない**（`docs/verification-coverage.md`
/// の規律）。**`(info)` の行に出すだけである。**
///
/// # 見出しを出すたびに、前の項目の所要を出す
///
/// **項目の終わりを呼ぶ側に書かせない。** **`--full` の項目は51箇所から
/// 見出しを出しており、終わりを1つずつ書かせると、書き忘れた項目だけが
/// 黙って消える。** **次の見出しが前の項目の終わりである。**
static ITEM_CLOCK: std::sync::Mutex<Option<(Instant, String)>> = std::sync::Mutex::new(None);

/// 項目の見出しを出し、時計を始める（VIEW-b の後）。
fn begin_item(label: &str) {
    finish_item();
    ITEMS_DONE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    stop_if_over_the_time_limit();
    println!("=== xtask check: {label}");
    if let Ok(mut clock) = ITEM_CLOCK.lock() {
        *clock = Some((Instant::now(), label.to_string()));
    }
}

/// 上限を過ぎていたら、そこで止める（VIEW-c の後）。
///
/// # 「切れた」と「落ちた」を分ける
///
/// **締めの行に、走った数と落ちた数を出す。** **落ちた数が 0 なら
/// 「ここまでは緑」と言える。** **終了の値も分ける**——
/// **検査の失敗は 1、上限で切れたのは 3 である。**
fn stop_if_over_the_time_limit() {
    let over = TIME_LIMIT
        .lock()
        .ok()
        .and_then(|limit| *limit)
        .is_some_and(|(started, limit)| started.elapsed() > limit);
    if !over {
        return;
    }
    let done = ITEMS_DONE.load(std::sync::atomic::Ordering::SeqCst) - 1;
    let failed = FAILED_SO_FAR.load(std::sync::atomic::Ordering::SeqCst);
    println!(
        "xtask check: stopped at the time limit ({} minute(s)). {done} check(s) ran and {failed} \
         failed - this is the limit, not a failing check. Raise FULL_TIME_LIMIT if the machine \
         got slower.",
        FULL_TIME_LIMIT.as_secs() / 60
    );
    std::process::exit(3);
}

/// 走っている項目の所要を出す（VIEW-b の後）。**走っていなければ何もしない。**
fn finish_item() {
    let taken = ITEM_CLOCK.lock().ok().and_then(|mut clock| clock.take());
    if let Some((started, label)) = taken {
        println!(
            "(info) item time: {:.1}s for {label}",
            started.elapsed().as_secs_f64()
        );
    }
}

fn run_regression(
    name: &str,
    failed: &mut Failures,
    retries: &mut Vec<String>,
    mut body: impl FnMut() -> Result<()>,
) {
    begin_item(name);
    let first = body();
    let error = match first {
        Ok(()) => {
            println!("--- {name}: OK");
            return;
        }
        Err(error) => error,
    };

    let did_not_start = format!("{error:#}").contains(DID_NOT_START_MARKER);
    if !did_not_start {
        println!("--- {name}: FAILED ({error:#})");
        failed.push(name.to_string());
        return;
    }

    println!("--- {name}: the target did not start; retrying once (environment, not the code)");
    retries.push(name.to_string());
    match body() {
        Ok(()) => println!("--- {name}: OK (on the retry)"),
        Err(error) => {
            println!("--- {name}: FAILED ({error:#})");
            failed.push(name.to_string());
        }
    }
}

/// 建てたカーネルと、その構成の `OUT_DIR`（ES-d の手当て）。
///
/// # なぜ対で持つのか
///
/// **`OUT_DIR` は feature 構成ごとに別で、その下に `fs.img` が建つ。**
/// **ユーザープログラムを変える feature（`USER_PROGRAM_CFGS`）があるので、
/// 像は構成ごとに違うバイト列になる。**
///
/// **以前は `stage_esp` が「既定構成でもう一度 `cargo build` を走らせて
/// `OUT_DIR` を訊く」形だった**ので、**破壊ビルドを起こすときに、
/// 既定構成の像がディスクへ載っていた。** カーネルは自分が埋め込んだ像と
/// 突き合わせるので一致せず、**シェルが起きる前に停止していた**
/// （`docs/troubleshooting.md`）。
///
/// **建てた側と載せる側を対にして持てば、取り違えようが無い。**
struct KernelBuild {
    elf: PathBuf,
    out_dir: PathBuf,
}

/// カーネルを建て、ELF と `OUT_DIR` を返す。
///
/// **`OUT_DIR` は同じ `cargo` の出力から取る**——**別の呼び出しで訊くと、
/// 訊いた構成が違いうる**（それが上記の取り違えの原因だった）。
fn run_kernel_build(workspace_root: &Path, features: &[&str]) -> Result<KernelBuild> {
    let mut command = Command::new("cargo");
    command.current_dir(workspace_root).args([
        "build",
        "--target",
        KERNEL_TARGET,
        "-p",
        KERNEL_PACKAGE,
        "--bin",
        KERNEL_PACKAGE,
        "--message-format=json-render-diagnostics",
    ]);
    if !features.is_empty() {
        command.args(["--features", &features.join(",")]);
    }
    // **診断はそのまま流す。** `--message-format=json-render-diagnostics` は
    // 人が読む形の診断を stderr へ出すので、握らずに見せる。
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("failed to invoke cargo to build the kernel")?;
    if !output.status.success() {
        bail!("kernel build failed ({})", output.status);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut out_dir = None;
    for line in stdout.lines() {
        if !line.contains("\"reason\":\"build-script-executed\"") {
            continue;
        }
        // **kernel 以外のパッケージのビルドスクリプトも同じ形で出る。**
        if !line.contains(&format!("/{KERNEL_PACKAGE}#")) {
            continue;
        }
        if let Some(dir) = json_string_field(line, "out_dir") {
            out_dir = Some(PathBuf::from(dir));
        }
    }
    let out_dir = out_dir.context(
        "cargo did not report a build-script-executed message for the kernel; \
         cannot locate OUT_DIR",
    )?;

    let elf = workspace_root
        .join("target")
        .join(KERNEL_TARGET)
        .join("debug")
        .join(KERNEL_PACKAGE);
    if !elf.exists() {
        bail!(
            "kernel build reported success but {} is missing",
            elf.display()
        );
    }
    Ok(KernelBuild { elf, out_dir })
}

fn build_kernel_with_features(workspace_root: &Path, features: &[&str]) -> Result<KernelBuild> {
    run_kernel_build(workspace_root, features)
}

fn build_kernel(workspace_root: &Path, gfx_test: bool) -> Result<KernelBuild> {
    let features: &[&str] = if gfx_test {
        &[GFX_TEST_PATTERN_FEATURE]
    } else {
        &[]
    };
    run_kernel_build(workspace_root, features)
}

/// 打鍵の切り分け用のビルド（zi-e の後の手当て）。
///
/// # 何のために在るのか
///
/// **前景を Ring 3 が持っている間、届いたスキャンコードを記録する経路が無い。**
/// `drain_keyboard` は前景が取られていたら何も取り出さないので、
/// **キーが届かなかったのか、届いたが文字にならなかったのかが区別できない**
/// （`kernel/src/interrupts.rs`）。
///
/// **踏んだ**——JIS 配列を既定にした後、運用者の手元で `ろ` と `¥` が
/// 出なかった。**こちらの `sendkey` では両方とも `\` が出た**ので、
/// **物理キーから QEMU までの間で失われている見込みだが、それを運用者の
/// 画面で確かめる手立てが無かった。**
///
/// # 2 つの feature を組み合わせるだけである
///
/// **カーネルは変えていない。どちらも既に在る。**
/// `keep-steady-loop` はシェルを起こさないので、**カーネル自身の消費者が
/// 回り続ける**（前景が取られない）。`keyboard-raw-log` は取り出した
/// スキャンコードをそのままシリアルへ出す。
///
/// **したがって、押したキーが届いていれば `keyboard: scancode 0x..` が出る。
/// 出なければ、届いていない。**
fn build_kernel_for_key_probe(workspace_root: &Path, gfx_test: bool) -> Result<KernelBuild> {
    let mut features = vec!["keep-steady-loop", "keyboard-raw-log"];
    if gfx_test {
        features.push(GFX_TEST_PATTERN_FEATURE);
    }
    run_kernel_build(workspace_root, &features)
}

/// このプロジェクトで使う OVMF ビルド（Ubuntu の `ovmf` パッケージ）は、
/// ブート可能な `\EFI\BOOT\BOOTX64.EFI` を自動探索するのではなく、既定で
/// 組み込みの UEFI Interactive Shell を起動する（docs/troubleshooting.md
/// 参照。根本原因は未解明で、これは回避策）。そのシェルは起動直後に
/// `startup.nsh` を探して自動実行するため、それを使って明示的に
/// bootloader.efi をチェインロードする。
const STARTUP_NSH: &str = "FS0:\\EFI\\BOOT\\BOOTX64.EFI\r\n";

/// OVMF の既定の起動パス（`\EFI\BOOT\BOOTX64.EFI`）に bootloader.efi を配置
/// した ESP (EFI System Partition) 相当のディレクトリを用意する。QEMU の
/// `fat:` ドライバでこのディレクトリをそのまま仮想 FAT ドライブとして渡せる
/// ため、ディスクイメージファイルを別途作成する必要はない。
/// virtio ディスクの像の置き場所（S13-a）。
///
/// **`esp_dir` から導く**——起動に使う成果物を 1 つの根（`target/`）に集め、
/// [`stage_esp`]（作る側）と [`qemu_launch_args`]（渡す側）が同じ導出を使う。
fn disk_image_path(esp_dir: &Path) -> PathBuf {
    esp_dir
        .parent()
        .expect("the esp dir always lives under target/")
        .join("disk0.img")
}

/// `stage_esp` が `disk0.img` をどう扱うか（P-a）。
///
/// # なぜ入口を分けるのか
///
/// **`stage_esp` の呼び手は 12 箇所ある**（実測。2026-08-28）。**引数を足すと
/// 全部が動く。** **持ち越す起動は新しい道なので、既存の 12 箇所の振る舞いを
/// 1 つも変えずに足せる形にする。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DiskImage {
    /// 建てた像で作り直す。**既存の 12 箇所はすべてこれである。**
    Rebuild,
    /// 在るものをそのまま使う（P-a）。**無ければ作り直す。**
    Keep,
}

/// `disk0.img` を作り直さずに ESP だけを積む（P-a）。
///
/// **2 度目の起動で使う。** **1 度目が書いた装置の中身を、そのまま持ち越す。**
fn stage_esp_keeping_the_disk(
    workspace_root: &Path,
    bootloader_efi: &Path,
    kernel: &KernelBuild,
) -> Result<PathBuf> {
    stage_esp_with_disk(workspace_root, bootloader_efi, kernel, DiskImage::Keep)
}

fn stage_esp(
    workspace_root: &Path,
    bootloader_efi: &Path,
    kernel: &KernelBuild,
) -> Result<PathBuf> {
    stage_esp_with_disk(workspace_root, bootloader_efi, kernel, DiskImage::Rebuild)
}

fn stage_esp_with_disk(
    workspace_root: &Path,
    bootloader_efi: &Path,
    kernel: &KernelBuild,
    disk: DiskImage,
) -> Result<PathBuf> {
    let kernel_elf = kernel.elf.as_path();
    let esp_dir = workspace_root.join("target").join("esp");
    let boot_dir = esp_dir.join("EFI").join("BOOT");
    fs::create_dir_all(&boot_dir)
        .with_context(|| format!("failed to create {}", boot_dir.display()))?;

    let boot_efi = boot_dir.join("BOOTX64.EFI");
    fs::copy(bootloader_efi, &boot_efi).with_context(|| {
        format!(
            "failed to copy {} to {}",
            bootloader_efi.display(),
            boot_efi.display()
        )
    })?;

    let startup_nsh = esp_dir.join("startup.nsh");
    fs::write(&startup_nsh, STARTUP_NSH)
        .with_context(|| format!("failed to write {}", startup_nsh.display()))?;

    // bootloader 側の ELF ローダー（bootloader/src/loader.rs）がここから
    // 読み込む（M2-0c）。
    let kernel_dir = esp_dir.join("zaytos");
    fs::create_dir_all(&kernel_dir)
        .with_context(|| format!("failed to create {}", kernel_dir.display()))?;
    let staged_kernel_elf = kernel_dir.join("kernel.elf");
    fs::copy(kernel_elf, &staged_kernel_elf).with_context(|| {
        format!(
            "failed to copy {} to {}",
            kernel_elf.display(),
            staged_kernel_elf.display()
        )
    })?;

    // virtio ディスクの像を作り直す（S13-a）。**起こすたびに、である**——
    // ゲストが書く可変の共有状態なので、残すと「前の項目が書いた中身を
    // 次の項目が見る」形になる（`troubleshooting.md` 2026-08-17 の族。
    // 書き込みが入る S13-e の手当ては `deferred-decisions.md` の行にある）。
    //
    // **中身は建てた ext2 の像そのものである（S13-c。ADR-0034）。**
    // S13-b では模様を置いていたが、S13-c でカーネルが像をこのディスクから
    // ロードするようになった。像は決定的（`build.rs` が時刻を潰す）なので、
    // どの feature 構成でも同じバイト列になる。大きさも像と同じにする
    // （16MiB に伸ばす根拠が無くなった）。
    let disk_image = disk_image_path(&esp_dir);
    // **載せる像は、いま積んだカーネルが埋め込んでいるものと同じである**
    // （[`KernelBuild`] の doc）。**別の構成の像を載せると、カーネルの
    // 突き合わせが落ちて、シェルが起きる前に停止する。**
    let built = kernel.out_dir.join(FS_IMAGE_NAME);
    // **持ち越す起動では、在るものに触れない（P-a）。**
    // **無ければ作り直す**——1 度目の起動はここを通る。
    if disk == DiskImage::Rebuild || !disk_image.exists() {
        fs::copy(&built, &disk_image).with_context(|| {
            format!(
                "failed to copy {} to {}",
                built.display(),
                disk_image.display()
            )
        })?;
    } else {
        println!(
            "persist: kept {} as it is (not rebuilt from {})",
            disk_image.display(),
            built.display()
        );
    }

    Ok(esp_dir)
}

/// 出力を解析する外の道具の一覧（e-4 の後の手当て）。
///
/// **ここに載っているものは [`external_tool`] を通して呼ぶこと。**
/// **`cargo xtask check` の静的検査が、直に `Command::new` していないかを見る。**
///
/// # 言語は機械で守れる。**文言は守れない**
///
/// **この検査が守るのは「言語を固定して呼ぶこと」までである。**
/// **道具の版が上がって文言が変われば、その語を探している判定が壊れる**
/// ——**それは機械では捕まらない。**
///
/// **どの判定がどの文言に乗っているかは `docs/verification-coverage.md` の
/// 「外の道具の文言に乗っている判定」に集めてある。** **版を上げるときは
/// あの節を読むこと。**
const PARSED_EXTERNAL_TOOLS: &[&str] =
    &["e2fsck", "dumpe2fs", "debugfs", "mke2fs", "nm", "objdump"];

/// 出力を解析する外の道具を呼ぶ（e-4 の後の手当て）。**言語を固定する。**
///
/// # なぜ 1 箇所へ寄せたのか
///
/// **外の道具の出力は環境の言語で訳される。** 実測で踏んだ——`dumpe2fs` の
/// 群の見出しが `グループ 0:` で出て、**「群の節に入ってから読む」が成り立たず、
/// superblock の「空きの数」419 を最初の空きブロック（正しくは 93）として
/// 拾った**（`docs/troubleshooting.md`）。**どちらも「もっともらしい数」なので、
/// 数だけを見ていると黙って通る。**
///
/// **付け忘れは静的検査が見る**（[`PARSED_EXTERNAL_TOOLS`]）。
fn external_tool(name: &str) -> Command {
    let mut command = Command::new(name);
    // **英語で出させる。** **解析しているのは見出しの語と数の並びである。**
    command.env("LC_ALL", "C");
    command
}

fn workspace_root() -> Result<PathBuf> {
    // xtask は常に `<workspace_root>/xtask` に置かれ、cargo run 経由で起動される
    // ため、CARGO_MANIFEST_DIR の親をワークスペースルートとみなせる。
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .parent()
        .map(Path::to_path_buf)
        .context("failed to resolve workspace root from CARGO_MANIFEST_DIR")
}

/// OVMF の変数領域 (NVRAM) は QEMU が起動時に書き込むため、パッケージ配布物を
/// そのまま渡さず target/ovmf/ 配下に書き込み可能なコピーを用意する。
fn prepare_ovmf_vars(workspace_root: &Path) -> Result<PathBuf> {
    let ovmf_dir = workspace_root.join("target").join("ovmf");
    fs::create_dir_all(&ovmf_dir)
        .with_context(|| format!("failed to create {}", ovmf_dir.display()))?;

    // **毎回テンプレートから作り直す。** OVMF はこの varstore を書き換える
    // ので、使い回すと起動項目（`EFI Internal Shell`、PXE、HTTP boot）が
    // 蓄積し、実行を重ねるほど条件が変わっていく。実際、初回だけコピーする
    // 作りだったときは `--full` の 3 回中 3 回で「OVMF がシェルへ落ちる」
    // 事象が起き、毎回作り直すと 2 回中 0 件になった
    // （docs/troubleshooting.md 参照）。
    //
    // 原因をバイト単位まで特定したわけではない。ただ、テストのたびに状態が
    // 持ち越される作りは、それ自体が「実行ごとに条件が変わる」という観測上の
    // 不確実性であり、無くしておく価値がある。費用はファイルコピー 1 回で、
    // 失うのは前回の起動で OVMF が覚えた設定だけである。ZaytOS は毎回同じ
    // ESP から同じ構成で起動するので、覚えていてほしいものは無い。
    let vars_copy = ovmf_dir.join("OVMF_VARS_4M.fd");
    {
        fs::copy(OVMF_VARS_TEMPLATE_PATH, &vars_copy).with_context(|| {
            format!(
                "failed to copy OVMF vars template from {} (is the `ovmf` package installed? `apt install ovmf`)",
                OVMF_VARS_TEMPLATE_PATH
            )
        })?;
    }
    Ok(vars_copy)
}

/// QEMU 起動引数を組み立てる。
///
/// `-no-reboot -no-shutdown -d int,cpu_reset` は常時付与する: これらが無いと
/// 致命的例外発生時に QEMU が無言でリブートを
/// 繰り返し、原因を外部から観測できなくなる。
fn qemu_launch_args(opts: &QemuLaunchOptions) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        // デフォルトの i440FX/PIIX チップセット（レガシー IDE を持つ）を使う。
        // q35 では OVMF がドライブを既定の起動先として自動認識しなかった
        // ため（ADR-0007）、単純な IDE 接続のほうが確実である。
        "-m".into(),
        "256M".into(),
        "-drive".into(),
        format!(
            "if=pflash,format=raw,readonly=on,file={}",
            opts.ovmf_code.display()
        )
        .into(),
        "-drive".into(),
        format!("if=pflash,format=raw,file={}", opts.ovmf_vars.display()).into(),
        // ESP 相当のディレクトリを仮想 FAT ドライブとして渡す。OVMF は既定の
        // 起動パス `\EFI\BOOT\BOOTX64.EFI` を自動的に見つけて起動する。
        "-drive".into(),
        format!("format=raw,file=fat:rw:{}", opts.esp_dir.display()).into(),
        // virtio-blk ディスク（S13-a で常設にした）。起動可能な中身を持たない
        // ので、OVMF の起動順は乱れない（実測）。**像は `stage_esp` が QEMU を
        // 起こすたびに作り直す**——ゲストが書く可変の共有状態で、`target/esp`
        // と同じ族である（`deferred-decisions.md` のディスク像の行）。
        "-drive".into(),
        format!(
            "if=none,id=disk0,format=raw,file={}",
            disk_image_path(opts.esp_dir).display()
        )
        .into(),
        "-device".into(),
        "virtio-blk-pci,drive=disk0".into(),
        "-serial".into(),
        match opts.serial {
            SerialSink::Stdio => "stdio".into(),
            SerialSink::File(path) => format!("file:{}", path.display()).into(),
        },
        // 既定は none（ADR-0003: シリアルログを唯一の観測手段とする）。
        // `--gui` 指定時のみ実際のウィンドウ（WSLg 経由）を開く。
        // **窓を開けるときの既定は SDL である**（[`DisplayMode::Sdl`] に理由がある）。
        "-display".into(),
        match opts.display {
            DisplayMode::None => "none".into(),
            DisplayMode::Sdl => "sdl".into(),
            DisplayMode::Gtk => "gtk".into(),
        },
        "-no-reboot".into(),
        "-no-shutdown".into(),
        // KVM では例外・割り込みの大半が CPU 側で処理され QEMU を経由しない
        // ため、`-d int` の記録はほとんど残らない。デバッグは TCG（既定）、
        // 計測は KVM、という使い分けをすること（troubleshooting.md 参照）。
        "-d".into(),
        opts.debug_events.as_qemu_value().into(),
        // `-d` の出力先を明示的にファイルへ分離する。指定しない場合 QEMU 自身の
        // stderr に出て、`-serial stdio` のシリアル出力と混ざってしまい、
        // ターミナルでの可読性が大きく落ちる。
        "-D".into(),
        opts.debug_log.into(),
    ];

    match opts.accelerator {
        Accelerator::Tcg => {}
        Accelerator::Kvm => {
            args.push("-accel".into());
            args.push("kvm".into());
            // KVM では TSC がホストの実周波数で進む。計測用途では
            // ホスト側の TSC をそのまま見せる方が解釈しやすい。
            args.push("-cpu".into());
            args.push("host".into());
        }
    }

    if let Some(monitor_socket) = opts.monitor_socket {
        args.push("-monitor".into());
        args.push(format!("unix:{},server,nowait", monitor_socket.display()).into());
    }

    // **キーマップ（`-k`）は渡さない（zi-e）。**
    //
    // **一度渡してみて、外した。** JIS 固有キーが GTK で届かなかったので
    // `-k ja` を試したが、**効かなかった（実測）。** QEMU の文書のとおりで、
    // **`-k` が要るのは生のキーコードが取りにくい環境だけである**
    // （VNC・curses・一部の X11 サーバ）。**GTK も SDL も生のキーコードを使う。**
    //
    // **受理は証拠にならない**——**存在しない `-k zz` も同じく受理された**
    // （実測。起動時にキーマップを読まない）。**「渡しても落ちない」ことを
    // 「効いている」と読み違えない。**
    //
    // **効かないものを残さない。** 残すと、**「渡してあるのだから配列の
    // 問題ではない」という誤った証拠になる。**
    //
    // **届かない側は、窓の種類で解いた**（[`DisplayMode::Sdl`]）。

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options<'a>(serial: &'a SerialSink, debug_log: &'a Path) -> QemuLaunchOptions<'a> {
        QemuLaunchOptions {
            ovmf_code: Path::new("/x/CODE.fd"),
            ovmf_vars: Path::new("/y/VARS.fd"),
            esp_dir: Path::new("/z/esp"),
            serial,
            debug_log,
            display: DisplayMode::None,
            monitor_socket: None,
            accelerator: Accelerator::Tcg,
            debug_events: DebugEvents::IntAndCpuReset,
        }
    }

    fn joined_args(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn qemu_args_always_include_failure_visibility_flags() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a == "-no-reboot"));
        assert!(joined.iter().any(|a| a == "-no-shutdown"));

        let d_pos = joined
            .iter()
            .position(|a| a == "-d")
            .expect("-d flag missing");
        assert_eq!(joined[d_pos + 1], "int,cpu_reset");
    }

    /// **手で触る経路だけが `int` を落とす（zi-e 前の手当て）。**
    ///
    /// **`-no-reboot` と `-no-shutdown` と `cpu_reset` は落ちない**——
    /// **沈黙の失敗を可視化する部分は残る**（`docs/coding-standards.md`）。
    /// **落ちるのは割り込みの列だけである。**
    #[test]
    fn manual_runs_record_cpu_reset_but_not_every_interrupt() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let mut opts = base_options(&SerialSink::Stdio, &debug_log);
        opts.debug_events = DebugEvents::CpuResetOnly;
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a == "-no-reboot"));
        assert!(joined.iter().any(|a| a == "-no-shutdown"));

        let d_pos = joined
            .iter()
            .position(|a| a == "-d")
            .expect("-d flag missing");
        assert_eq!(joined[d_pos + 1], "cpu_reset");
    }

    /// `info blockstats` の出力から `disk0` の行だけが拾えること（S13-c）。
    /// **標本は実測の出力そのものである**（`ide0-hd0` は ESP の FAT で、
    /// 拾ってはならない側）。
    #[test]
    fn disk0_rd_bytes_is_parsed_and_other_drives_are_ignored() {
        let text = "ide0-hd0: rd_bytes=7781888 wr_bytes=0 rd_operations=88
disk0: rd_bytes=8704 wr_bytes=0 rd_operations=11
ide1-cd0: rd_bytes=0 wr_bytes=0 rd_operations=0
";
        assert_eq!(parse_disk0_rd_bytes(text), Some(8704));
        assert_eq!(
            parse_disk0_rd_bytes(
                "floppy0: rd_bytes=1
"
            ),
            None
        );
    }

    /// `wr_bytes` も `disk0` の行だけから拾えること（S13-e。rd の対称）。
    #[test]
    fn disk0_wr_bytes_is_parsed_and_other_drives_are_ignored() {
        let text = "ide0-hd0: rd_bytes=7781888 wr_bytes=12345 rd_operations=88
disk0: rd_bytes=2105856 wr_bytes=2097152 rd_operations=524
";
        assert_eq!(parse_disk0_wr_bytes(text), Some(2097152));
        assert_eq!(
            parse_disk0_wr_bytes(
                "floppy0: wr_bytes=1
"
            ),
            None
        );
    }

    /// S13-b で足した 2 つの標識が、その行だけを落とすこと。
    ///
    /// **「消してはならない差が残ること」の検査である**——正規化の持ち越しの
    /// 行（`deferred-decisions.md`）が「規則を足すとき」に発火し、今回足した
    /// 2 つについて取り上げた。**checksum と capacity の行は、どちらの比較
    /// でも落ちてはならない**——落ちたら、S13-b の判定が参照から消える。
    #[test]
    fn the_two_s13b_markers_drop_their_lines_and_nothing_else() {
        let serial = "\
[INFO] virtio-blk: polling took 165 spin(s) (limit 20000000)\n\
[INFO] virtio-blk: host features=0x79007e54 (accepted 0)\n\
[INFO] virtio-blk: handshake: ACKNOWLEDGE -> DRIVER; capacity=32768 sector(s)\n\
[INFO] virtio-blk: read sector 0: 512 byte(s) requested, used.len=513, status=0 (OK); checksum=0x01195f6d first bytes=[00, 01, 02, 03, 04, 05, 06, 07]\n";
        // 参照との比較（コア数の行は残す側）。
        let kept = normalize_boot_log(serial, false);
        assert!(kept.iter().all(|l| !l.contains("polling took")));
        assert!(kept.iter().any(|l| l.contains("host features")));
        assert!(kept.iter().any(|l| l.contains("checksum=")));
        assert!(kept.iter().any(|l| l.contains("capacity=")));
        // smp 比較（コア数の行も落とす側）では features も落ちる。
        let cross = normalize_boot_log(serial, true);
        assert!(cross.iter().all(|l| !l.contains("host features")));
        assert!(cross.iter().any(|l| l.contains("checksum=")));
        assert!(cross.iter().any(|l| l.contains("capacity=")));
    }

    /// カーネルの列挙の行が拾え、同じ接頭辞の別の行が混ざらないこと（S13-a）。
    #[test]
    fn kernel_pci_lines_are_parsed_and_other_pci_lines_are_ignored() {
        let serial = "\
[INFO] pci: bus 0 device 0 function 0: 8086:1237 class=0x06 subclass=0x00 header=0x00 irq line=0 pin=0 bars=[0x0 0x0 0x0 0x0 0x0 0x0]\n\
[INFO] pci: bus 0 device 4 function 0: 1af4:1001 class=0x01 subclass=0x00 header=0x00 irq line=11 pin=1 bars=[0xc001 0x810a0000 0x0 0x0 0xc000000c 0x0]\n\
[INFO] pci: enumeration complete: 2 function(s) on bus 0, virtio-blk (vendor 0x1af4 device 0x1001 or 0x1041) found 1 time(s)\n";
        let parsed = parse_kernel_pci_lines(serial);
        assert_eq!(
            parsed,
            vec![
                (0, 0, 0, "8086:1237".to_string()),
                (0, 4, 0, "1af4:1001".to_string()),
            ]
        );
    }

    /// `info pci` の出力が拾え、subsystem の行が混ざらないこと（S13-a）。
    /// **標本は実測の出力そのものである**（QEMU 8 系、i440FX）。
    #[test]
    fn info_pci_output_is_parsed_and_subsystem_lines_are_ignored() {
        let text = "\
  Bus  0, device   0, function 0:\r\n\
    Host bridge: PCI device 8086:1237\r\n\
      PCI subsystem 1af4:1100\r\n\
      id \"\"\r\n\
  Bus  0, device   4, function 0:\r\n\
    SCSI controller: PCI device 1af4:1001\r\n\
      PCI subsystem 1af4:0002\r\n\
      IRQ 11, pin A\r\n\
      BAR0: I/O at 0xc000 [0xc07f].\r\n\
      id \"\"\r\n";
        let parsed = parse_info_pci(text);
        assert_eq!(
            parsed,
            vec![
                (0, 0, 0, "8086:1237".to_string()),
                (0, 4, 0, "1af4:1001".to_string()),
            ]
        );
        assert_eq!(count_virtio_blk(&parsed), 1);
    }

    /// virtio ディスクが常設であること（S13-a）。**像の経路は `stage_esp` の
    /// 作る側と同じ導出**（[`disk_image_path`]）であることも、ここで固定する。
    #[test]
    fn qemu_args_always_include_the_virtio_disk() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined
            .iter()
            .any(|a| a == "if=none,id=disk0,format=raw,file=/z/disk0.img"));
        let device_pos = joined
            .iter()
            .position(|a| a == "virtio-blk-pci,drive=disk0")
            .expect("virtio-blk-pci device missing");
        assert_eq!(joined[device_pos - 1], "-device");
    }

    /// 手で起こすときの像の扱い（P-c-3）。
    ///
    /// **主張は 2 つあり、渡らない側が主である。**
    ///
    /// - **`--manual` のときは持ち越す**（人が触るときの既定）
    /// - **`--manual` でないときは持ち越さない**——**明示で頼まない限り。**
    ///   **こちらが要る**——**検査は毎回同じ像から始まる必要があり、
    ///   「うっかり持ち越す」が起きると、汚れた像の上で走った検査が
    ///   緑になる。** **落ちるのではなく緑になるので、気づけない。**
    #[test]
    fn only_the_manual_run_keeps_the_disk_by_default() {
        // manual, keep, rebuild -> 期待
        let cases: &[(bool, bool, bool, DiskImage)] = &[
            (false, false, false, DiskImage::Rebuild),
            (true, false, false, DiskImage::Keep),
            (false, true, false, DiskImage::Keep),
            (true, false, true, DiskImage::Rebuild),
            (false, true, true, DiskImage::Rebuild),
            (true, true, false, DiskImage::Keep),
            (true, true, true, DiskImage::Rebuild),
        ];
        for &(manual, keep, rebuild, expected) in cases {
            assert_eq!(
                disk_for_run(manual, keep, rebuild),
                expected,
                "manual={manual} keep={keep} rebuild={rebuild}"
            );
        }

        // **渡らない側を、旗の組み合わせを尽くして主張する。**
        // **`--manual` でも `--keep-disk` でもない組み合わせは 2 つで、
        // どちらも作り直しである。**
        let kept: Vec<(bool, bool, bool)> = [false, true]
            .into_iter()
            .flat_map(|m| {
                [false, true]
                    .into_iter()
                    .flat_map(move |k| [false, true].into_iter().map(move |r| (m, k, r)))
            })
            .filter(|&(m, k, r)| disk_for_run(m, k, r) == DiskImage::Keep)
            .collect();
        assert!(
            kept.iter().all(|&(m, k, _)| m || k),
            "the disk was kept without --manual and without --keep-disk: {kept:?}"
        );
    }

    /// 索引と `.claude/` の対応を、**両向きとも**見ていることを確かめる。
    ///
    /// **`git` を動かさない。** 判定は純粋関数で、入力は索引の本文と追跡下の
    /// 一覧の 2 つだけである（`.claude/rules/temporary-changes.md` の
    /// 「検査そのものを試すときは、リポジトリを動かさない」）。
    #[test]
    fn the_agent_index_check_looks_both_ways() {
        let tracked = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // 揃っている。**ディレクトリの言及は指し先として数えない。**
        let index = "本体は `.claude/rules/a.md` にある。`.claude/` の整備で移した。";
        assert!(find_agent_index_mismatches(index, &tracked(&[".claude/rules/a.md"])).is_empty());

        // 順向き: 索引が追跡下に無いものを指している（改名がこの形で出る）。
        // **同時に逆向きも出る**——改名前の名前は索引から消えているためである。
        let renamed = find_agent_index_mismatches(
            "本体は `.claude/rules/b.md` にある。",
            &tracked(&[".claude/rules/a.md"]),
        );
        assert_eq!(renamed.len(), 2, "{renamed:?}");
        assert!(
            renamed[0].contains("names .claude/rules/b.md"),
            "{renamed:?}"
        );
        assert!(renamed[1].contains("never names it"), "{renamed:?}");

        // 逆向きだけ: 追跡下に在るのに、索引が名前を出していない。
        let unlisted = find_agent_index_mismatches(
            "本体は `.claude/rules/a.md` にある。",
            &tracked(&[".claude/rules/a.md", ".claude/settings.json"]),
        );
        assert_eq!(unlisted.len(), 1, "{unlisted:?}");
        assert!(
            unlisted[0].contains(".claude/settings.json"),
            "{unlisted:?}"
        );
    }

    /// 規則ごとに、違反する形が捕まることを見る（`CLAUDE.md` の「コミットメッセージ」）。
    ///
    /// **`git` を動かさない。** 実際のコミットで確かめる形は後始末に
    /// `git reset --hard` が要り、**未コミットの変更を巻き込む**
    /// （この検査を書いている最中に実際に踏んだ）。
    #[test]
    fn the_commit_message_rules_catch_each_shape() {
        let both = |m: &str| commit_message_findings("0000000", m, true, true);

        // 接頭辞が集合の外。
        assert_eq!(both("wip: 集合の外").len(), 1);
        assert!(both("wip: 集合の外")[0].contains("prefix"));
        // 接頭辞そのものが無い。
        assert_eq!(both("接頭辞が無い").len(), 1);

        // 本文があるのに 2 行目が空でない。
        let joined = both("docs: 件名\n本文をすぐ書く");
        assert_eq!(joined.len(), 1, "{joined:?}");
        assert!(joined[0].contains("second line"));

        // 本文が 1 行、および 6 行。
        assert!(both("docs: 件名\n\n一行だけ。")[0].contains("1 line(s)"));
        let six = "docs: 件名\n\n1。\n2。\n3。\n4。\n5。\n6。";
        assert!(both(six)[0].contains("6 line(s)"));

        // 和文と英数字の間の空白。
        let gap = both("docs: halt_forever の数");
        assert_eq!(gap.len(), 1, "{gap:?}");
        assert!(gap[0].contains("Japanese and ASCII"));
    }

    /// **落ちてはならない側も見る。** 通るべき形で findings が出ないこと。
    #[test]
    fn the_commit_message_rules_pass_the_shapes_that_are_allowed() {
        let both = |m: &str| commit_message_findings("0000000", m, true, true);

        // 件名だけ。**「1 行で足りるなら 1 行」は機械では見ないので、通す。**
        assert!(both("docs: 件名だけのコミット").is_empty());
        // 本文が 2 行から 5 行。
        assert!(both("docs: 件名\n\n1。\n2。").is_empty());
        assert!(both("feat: 件名\n\n1。\n2。\n3。\n4。\n5。").is_empty());
        // 末尾の改行が余分にあっても数に入らない。
        assert!(both("fix: 件名\n\n1。\n2。\n\n").is_empty());
        // 7 つの接頭辞すべて。
        for prefix in COMMIT_SUBJECT_PREFIXES {
            assert!(both(&format!("{prefix}: 件名")).is_empty(), "{prefix}");
        }
    }

    /// **範囲の旗が効くこと。** 当てない規則は、違反していても出ない。
    #[test]
    fn the_range_flags_turn_the_rules_off() {
        let long = "docs: 件名\n\n1。\n2。\n3。\n4。\n5。\n6。";
        assert!(commit_message_findings("0000000", long, false, true).is_empty());
        let gap = "docs: halt_forever の数";
        assert!(commit_message_findings("0000000", gap, true, false).is_empty());
    }

    #[test]
    fn qemu_args_reference_given_ovmf_and_esp_paths() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a.contains("/x/CODE.fd")));
        assert!(joined.iter().any(|a| a.contains("/y/VARS.fd")));
        assert!(joined.iter().any(|a| a.contains("fat:rw:/z/esp")));
    }

    #[test]
    fn qemu_args_use_serial_file_sink_when_requested() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let sink = SerialSink::File(PathBuf::from("/tmp/serial.log"));
        let opts = base_options(&sink, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a == "file:/tmp/serial.log"));
    }

    #[test]
    fn qemu_args_separate_debug_log_from_serial() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        let d_capital_pos = joined
            .iter()
            .position(|a| a == "-D")
            .expect("-D flag missing");
        assert_eq!(joined[d_capital_pos + 1], "/z/qemu-debug.log");
    }

    #[test]
    fn qemu_args_default_display_is_none() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        let pos = joined
            .iter()
            .position(|a| a == "-display")
            .expect("-display flag missing");
        assert_eq!(joined[pos + 1], "none");
    }

    /// **窓を開けるときの既定は SDL である（zi-e）。**
    ///
    /// # なぜ既定を固定するのか
    ///
    /// **GTK は JIS 固有キー（`ろ` = `0x73` / `¥` = `0x7D`）を落とす**
    /// （実測。`docs/troubleshooting.md`）。**打つ人が居るのは窓を開けるときだけ
    /// なので、打てないキーがある側を既定に残さない。**
    ///
    /// **既定が戻ったことを、実際に打って気づく形にしない。**
    #[test]
    fn opening_a_window_selects_sdl_and_gtk_stays_available() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");

        for (display, wanted) in [
            (DisplayMode::None, "none"),
            (DisplayMode::Sdl, "sdl"),
            (DisplayMode::Gtk, "gtk"),
        ] {
            let mut opts = base_options(&SerialSink::Stdio, &debug_log);
            opts.display = display;
            let joined = joined_args(&qemu_launch_args(&opts));

            let pos = joined
                .iter()
                .position(|a| a == "-display")
                .expect("-display flag missing");
            assert_eq!(joined[pos + 1], wanted);
        }
    }

    /// **キーマップ（`-k`）はどの構成でも渡さない（zi-e）。**
    ///
    /// # 外したものが黙って戻らないようにする
    ///
    /// **一度 `-k ja` を渡し、効かないので外した**（実測。GTK でも SDL でも
    /// 生のキーコードを使うため）。**受理は証拠にならない**——`-k zz` も
    /// 受理されるので、**「渡しても落ちない」を「効いている」と読み違えうる。**
    ///
    /// **効かないものが残っていると、「渡してあるのだから配列の問題ではない」
    /// という誤った証拠になる。** 戻ったらここが落ちる。
    #[test]
    fn no_keyboard_layout_is_ever_passed() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");

        for display in [DisplayMode::None, DisplayMode::Sdl, DisplayMode::Gtk] {
            let mut opts = base_options(&SerialSink::Stdio, &debug_log);
            opts.display = display;
            let joined = joined_args(&qemu_launch_args(&opts));

            assert!(
                !joined.iter().any(|a| a == "-k"),
                "-k が渡っている（効かないことを実測してある）"
            );
        }
    }

    #[test]
    fn qemu_args_include_monitor_socket_when_requested() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let monitor_socket = PathBuf::from("/tmp/mon.sock");
        let mut opts = base_options(&SerialSink::Stdio, &debug_log);
        opts.monitor_socket = Some(&monitor_socket);
        let joined = joined_args(&qemu_launch_args(&opts));

        let pos = joined
            .iter()
            .position(|a| a == "-monitor")
            .expect("-monitor flag missing");
        assert_eq!(joined[pos + 1], "unix:/tmp/mon.sock,server,nowait");
    }

    #[test]
    fn qemu_args_omit_monitor_by_default() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(!joined.iter().any(|a| a == "-monitor"));
    }

    /// 既定は TCG のまま。これまでの全マイルストーンを TCG で検証してきて
    /// おり、既定を変えると挙動差の切り分け軸が増える（ADR-0015 Addendum）。
    #[test]
    fn qemu_args_do_not_enable_kvm_by_default() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let serial = SerialSink::Stdio;
        let args = joined_args(&qemu_launch_args(&base_options(&serial, &debug_log)));
        assert!(!args.iter().any(|a| a == "-accel"));
        assert!(!args.iter().any(|a| a == "kvm"));
    }

    #[test]
    fn qemu_args_enable_kvm_when_requested() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let serial = SerialSink::Stdio;
        let mut options = base_options(&serial, &debug_log);
        options.accelerator = Accelerator::Kvm;
        let args = joined_args(&qemu_launch_args(&options));

        let accel = args.iter().position(|a| a == "-accel").expect("-accel");
        assert_eq!(args[accel + 1], "kvm");
        // 計測時は TSC の解釈を単純にするためホストの CPU をそのまま見せる。
        let cpu = args.iter().position(|a| a == "-cpu").expect("-cpu");
        assert_eq!(args[cpu + 1], "host");
    }

    /// KVM を選んでも、失敗を可視化するオプションは外さない。取れる情報は
    /// 減るが、外すと「何も出ない」理由が分からなくなる。
    #[test]
    fn kvm_still_keeps_the_failure_visibility_flags() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let serial = SerialSink::Stdio;
        let mut options = base_options(&serial, &debug_log);
        options.accelerator = Accelerator::Kvm;
        let args = joined_args(&qemu_launch_args(&options));
        assert!(args.iter().any(|a| a == "-no-reboot"));
        assert!(args.iter().any(|a| a == "-no-shutdown"));
        assert!(args.iter().any(|a| a == "int,cpu_reset"));
    }

    #[test]
    fn panic_markers_present_requires_both_markers() {
        let dir = env::temp_dir().join(format!(
            "zaytos-xtask-test-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("serial.log");

        fs::write(&path, "boot ok\n").unwrap();
        assert!(!panic_markers_present(&path));

        fs::write(&path, format!("{PANIC_MARKER_HEADER} boom\n")).unwrap();
        assert!(!panic_markers_present(&path));

        fs::write(
            &path,
            format!("{PANIC_MARKER_HEADER} boom\n{PANIC_MARKER_HALT}\n"),
        )
        .unwrap();
        assert!(panic_markers_present(&path));

        let _ = fs::remove_dir_all(&dir);
    }
}
