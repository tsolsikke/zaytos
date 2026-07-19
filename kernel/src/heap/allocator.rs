//! ヒープ本体: 侵入型連結リストによる空きブロック管理と `GlobalAlloc`
//! 実装（M2-e）。
//!
//! 実ポインタでヒープメモリへ直接
//! 読み書きするため、意図的にホスト `cargo test` の対象にしていない。
//! 配置・分割・結合の判断ロジックは [`super::plan`] に分離済みで、
//! そちらをテストしている。

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::fmt::Write as _;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;

use common::serial::SerialPort;

use super::plan::{self, AllocPlan};

/// 割り込みが常に禁止されていることに依存した、最小限の同期ラッパー。
///
/// 「ロックのように見えるが中身は割り込み禁止だけ」という方針通り、
/// 中身は最小限にとどめてある。
struct Locked<T> {
    inner: UnsafeCell<T>,
}

impl<T> Locked<T> {
    const fn new(value: T) -> Self {
        Self {
            inner: UnsafeCell::new(value),
        }
    }

    /// 排他アクセス用の可変参照を得る。
    ///
    /// # Safety 的な前提（呼び出し側ではなく、この型自体の前提）
    /// 現在は kernel 全体で割り込みが常時禁止されており
    /// （`docs/architecture.md` §6.5）、かつシングルコア前提であるため、
    /// この可変参照を保持している間に他の実行文脈が同時にこの値へ
    /// アクセスすることはない。**M4 で割り込みを有効化する際は、この前提が
    /// 崩れるため、実装を cli/sti の保存・復元、またはスピンロックへ
    /// 差し替える必要がある**（`docs/architecture.md` §6.5 の見直し事項）。
    // `&self` から `&mut T` を返すのはこのラッパーの意図した振る舞い
    // （`UnsafeCell` による内部可変性）であり、単一コア・割り込み禁止と
    // いう前提のもとでのみ安全。
    #[allow(clippy::mut_from_ref)]
    fn lock(&self) -> &mut T {
        unsafe { &mut *self.inner.get() }
    }
}

// SAFETY: 複数の実行文脈からの同時アクセスは、シングルコア前提かつ
// kernel 全体で割り込みが常時禁止されている（`docs/architecture.md` §6.5）
// ことによって防がれている。M4 で割り込みを有効化する際は、この前提が
// 崩れるため `Locked` の実装ごと必ず見直すこと。
unsafe impl<T> Sync for Locked<T> {}

/// 空きブロックの先頭に書き込む、侵入型連結リストのノード。
#[repr(C)]
struct FreeBlockNode {
    size: u64,
    next: Option<NonNull<FreeBlockNode>>,
}

/// 空きブロックとして分離してよい最小サイズ。`FreeBlockNode` 自体が
/// 書き込めない大きさのブロックを空きリストに残すと、次にそのブロックを
/// 読み書きした際にヒープを破壊するため、静的アサーションで強制する。
const MIN_BLOCK_SIZE: u64 = size_of::<FreeBlockNode>() as u64;
const _: () = assert!(
    MIN_BLOCK_SIZE >= size_of::<FreeBlockNode>() as u64,
    "MIN_BLOCK_SIZE must be able to hold a FreeBlockNode"
);

/// 確保済みブロックの直前に書き込むヘッダ。
#[repr(C)]
struct AllocatedBlockHeader {
    /// 二重解放・不正ポインタ解放・ヒープ破壊を検出するためのマジック値。
    magic: u64,
    /// 元の空きブロックの絶対開始アドレス（アラインメント調整で生じた
    /// 前方の隙間を含む）。解放時にこの位置からまるごと空きリストへ戻す。
    block_start: u64,
    /// `block_start` から数えた、消費領域全体の大きさ。
    block_size: u64,
    /// `alloc` 時に要求された `layout.size()`。`dealloc` に渡される
    /// `layout` との突き合わせに使う（呼び出し側の不整合検出）。
    requested_size: u64,
}

const HEADER_MAGIC: u64 = 0x5A61_7974_4845_4144; // "ZaytHEAD" 由来の目印。

struct HeapState {
    free_list_head: Option<NonNull<FreeBlockNode>>,
}

impl HeapState {
    const fn empty() -> Self {
        Self {
            free_list_head: None,
        }
    }

    fn free_bytes(&self) -> u64 {
        let mut total = 0u64;
        let mut current = self.free_list_head;
        while let Some(node_ptr) = current {
            // SAFETY: 空きリストのノードはすべて `init`/`dealloc` が
            // 書き込んだ有効な `FreeBlockNode` を指す。
            let node = unsafe { node_ptr.as_ref() };
            total += node.size;
            current = node.next;
        }
        total
    }

    fn free_block_count(&self) -> u64 {
        let mut count = 0u64;
        let mut current = self.free_list_head;
        while let Some(node_ptr) = current {
            // SAFETY: 上記と同様。
            let node = unsafe { node_ptr.as_ref() };
            count += 1;
            current = node.next;
        }
        count
    }
}

/// ヒープ本体。`#[global_allocator]` として登録して使う。
pub struct LockedHeap {
    state: Locked<HeapState>,
}

impl LockedHeap {
    pub const fn empty() -> Self {
        Self {
            state: Locked::new(HeapState::empty()),
        }
    }

    /// ヒープの初期アリーナを1つ登録する。
    ///
    /// # Safety
    /// - `[start, start + size)` は、他の誰にも使われていない、有効な
    ///   （マップ済みの）物理メモリ範囲であること（呼び出し側が
    ///   `paging::plan::MappedRanges::contains_range` 等で確認しておく）。
    /// - このヒープに対して一度だけ呼び出すこと（複数回呼ぶと最初の
    ///   アリーナの情報が失われ、二重に「空き」として扱われる）。
    pub unsafe fn init(&self, start: u64, size: u64) {
        let node_ptr = start as *mut FreeBlockNode;
        // SAFETY: 呼び出し元契約により `[start, start+size)` は有効。
        unsafe {
            node_ptr.write(FreeBlockNode { size, next: None });
        }
        self.state.lock().free_list_head = NonNull::new(node_ptr);
    }

    pub fn free_bytes(&self) -> u64 {
        self.state.lock().free_bytes()
    }

    pub fn free_block_count(&self) -> u64 {
        self.state.lock().free_block_count()
    }
}

impl Default for LockedHeap {
    fn default() -> Self {
        Self::empty()
    }
}

/// パニックハンドラ（`kernel/src/panic.rs`）と同様、ヒープを一切使わずに
/// シリアルへ直接書く（OOM 状態でヒープ経由のログ機構を使うと、それ自体が
/// 再帰的に確保を試みかねないため）。
fn log_directly_to_serial(args: core::fmt::Arguments<'_>) {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = serial.write_fmt(args);
    let _ = serial.write_str("\n");
}

// SAFETY: `alloc`/`dealloc` はヒープアリーナ（`init` で登録した、呼び出し
// 元が有効性を保証した領域）の中だけを読み書きする。`Locked` により
// （現状の割り込み常時禁止という前提のもとで）排他アクセスを保証している。
unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            // GlobalAlloc の契約上、呼び出し側は非ゼロサイズを保証する
            // はずだが（Rust core は ZST に対してこの関数を呼ばない）、
            // 契約違反を握りつぶさず検出する。
            log_directly_to_serial(format_args!(
                "[ERROR] heap: alloc called with zero-sized layout (align={})",
                layout.align()
            ));
            panic!("heap: alloc called with a zero-sized Layout (contract violation)");
        }

        let header_size = size_of::<AllocatedBlockHeader>() as u64;
        let header_align = align_of::<AllocatedBlockHeader>() as u64;
        let requested_size = layout.size() as u64;
        let requested_align = layout.align() as u64;

        let state = self.state.lock();

        let mut prev: Option<NonNull<FreeBlockNode>> = None;
        let mut current = state.free_list_head;

        while let Some(node_ptr) = current {
            // SAFETY: 空きリストのノードは有効な `FreeBlockNode` を指す。
            let node = unsafe { node_ptr.as_ref() };
            let region_start = node_ptr.as_ptr() as u64;
            let region_end = region_start + node.size;
            let next = node.next;

            if let Some(found_plan) = plan::plan_allocation(
                region_start,
                region_end,
                header_size,
                header_align,
                requested_size,
                requested_align,
                MIN_BLOCK_SIZE,
            ) {
                Self::consume_node(state, prev, next, found_plan);

                let header = AllocatedBlockHeader {
                    magic: HEADER_MAGIC,
                    block_start: found_plan.block_start,
                    block_size: found_plan.block_size,
                    requested_size,
                };
                // SAFETY: `found_plan.header_addr` は `region_start` から
                // `region_end` 内(4KiB以上の恒等マッピング済み領域)にあり、
                // このリクエスト用に消費領域として確保したばかりで他の
                // 参照は存在しない。
                unsafe {
                    (found_plan.header_addr as *mut AllocatedBlockHeader).write(header);
                }
                return found_plan.user_addr as *mut u8;
            }

            prev = current;
            current = next;
        }

        log_directly_to_serial(format_args!(
            "[ERROR] heap: allocation failed: requested size={} align={} \
             free_bytes={} free_blocks={}",
            requested_size,
            requested_align,
            state.free_bytes(),
            state.free_block_count(),
        ));
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let header_size = size_of::<AllocatedBlockHeader>() as u64;
        let header_ptr = (ptr as u64 - header_size) as *const AllocatedBlockHeader;
        // SAFETY: `ptr` は過去にこの `alloc` が返した値であるという
        // `dealloc` の呼び出し契約により、`ptr - header_size` にはこの
        // `alloc` が書き込んだ有効な `AllocatedBlockHeader` がある。
        let header = unsafe { header_ptr.read() };

        if header.magic != HEADER_MAGIC {
            log_directly_to_serial(format_args!(
                "[ERROR] heap: dealloc magic mismatch at {:#x} (found {:#x}, expected {:#x}); \
                 double free, heap corruption, or invalid pointer",
                ptr as u64, header.magic, HEADER_MAGIC
            ));
            panic!("heap: dealloc magic mismatch (corruption detected)");
        }
        if header.requested_size != layout.size() as u64 {
            log_directly_to_serial(format_args!(
                "[ERROR] heap: dealloc layout mismatch at {:#x} (layout.size()={}, \
                 recorded requested_size={})",
                ptr as u64,
                layout.size(),
                header.requested_size
            ));
            panic!("heap: dealloc layout size mismatch (caller/allocator inconsistency)");
        }

        let block_start = header.block_start;
        let block_size = header.block_size;

        let state = self.state.lock();

        // アドレス順の挿入位置 (prev, next) を探す。
        let mut prev: Option<NonNull<FreeBlockNode>> = None;
        let mut current = state.free_list_head;
        while let Some(node_ptr) = current {
            if node_ptr.as_ptr() as u64 > block_start {
                break;
            }
            prev = current;
            // SAFETY: 空きリストのノードは有効な `FreeBlockNode` を指す。
            current = unsafe { node_ptr.as_ref().next };
        }
        let next = current;

        // 毒埋めは、この直後に行うノード書き込みより必ず前に実施する。
        // 逆順にすると、これから書き込む `FreeBlockNode` の
        // `size`/`next` フィールド自体を毒値で潰してしまうため。
        #[cfg(feature = "heap-poison")]
        // SAFETY: `[block_start, block_start+block_size)` はこの解放対象
        // ブロックそのものであり、他に参照は存在しない。
        unsafe {
            core::ptr::write_bytes(block_start as *mut u8, 0xDEu8, block_size as usize);
        }

        // 直後のノードと隣接していれば結合する。
        let (merged_start, merged_size, merged_next) = match next {
            Some(next_nn) => {
                let next_start = next_nn.as_ptr() as u64;
                // SAFETY: 上記と同様。
                let next_node = unsafe { next_nn.as_ref() };
                match plan::merge_adjacent(block_start, block_size, next_start, next_node.size) {
                    Some((ms, msize)) => (ms, msize, next_node.next),
                    None => (block_start, block_size, next),
                }
            }
            None => (block_start, block_size, None),
        };

        let new_node_ptr = merged_start as *mut FreeBlockNode;
        // SAFETY: `merged_start` は解放したブロック(結合した場合は隣接
        // ノードを含む)の先頭であり、他に参照は存在しない。
        unsafe {
            new_node_ptr.write(FreeBlockNode {
                size: merged_size,
                next: merged_next,
            });
        }
        let new_node_nn = NonNull::new(new_node_ptr).expect("merged_start is never null");

        match prev {
            Some(mut prev_nn) => {
                let prev_start = prev_nn.as_ptr() as u64;
                // SAFETY: `prev_nn` は空きリストの既存ノード。
                let prev_size = unsafe { prev_nn.as_ref().size };
                match plan::merge_adjacent(prev_start, prev_size, merged_start, merged_size) {
                    Some((_, msize)) => {
                        // SAFETY: `prev_nn` はこのリストの生きているノード。
                        unsafe {
                            prev_nn.as_mut().size = msize;
                            prev_nn.as_mut().next = merged_next;
                        }
                    }
                    None => {
                        // SAFETY: 上記と同様。
                        unsafe {
                            prev_nn.as_mut().next = Some(new_node_nn);
                        }
                    }
                }
            }
            None => state.free_list_head = Some(new_node_nn),
        }
    }
}

impl LockedHeap {
    /// 見つかった空きノードをリストから外し、`plan.trailing_split` が
    /// あればその部分だけを同じ位置に新しいノードとして戻す。
    fn consume_node(
        state: &mut HeapState,
        prev: Option<NonNull<FreeBlockNode>>,
        next: Option<NonNull<FreeBlockNode>>,
        found_plan: AllocPlan,
    ) {
        let replacement = match found_plan.trailing_split {
            Some((start, size)) => {
                let node_ptr = start as *mut FreeBlockNode;
                // SAFETY: `start` はこれから確保する領域の直後にある、
                // まだ誰も参照していない残り領域の先頭。
                unsafe {
                    node_ptr.write(FreeBlockNode { size, next });
                }
                NonNull::new(node_ptr)
            }
            None => next,
        };

        match prev {
            Some(mut prev_nn) => {
                // SAFETY: `prev_nn` は空きリストの既存ノード。
                unsafe {
                    prev_nn.as_mut().next = replacement;
                }
            }
            None => state.free_list_head = replacement,
        }
    }
}
