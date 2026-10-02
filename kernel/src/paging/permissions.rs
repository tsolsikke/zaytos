//! ページの権限（2026-10-02。`ADR-0071` の手順 3）。**CPU に依らない言い方で持つ。**
//!
//! 「書けるか・実行できるか・ユーザーから届くか・キャッシュするか」を 1 つの型にし、ページテーブルのビットへ直すのは
//! CPU の置き場の 1 つの関数にする（x86_64 では `crate::arch::x86_64` の `paging::entry` の `leaf_flags` と
//! `table_flags`）。**権限を決める所は、ここの名前つきの関数だけにする。** 欄を直に書かせると、ページを足す所ごとに
//! 組み合わせが散らばり、どの組み合わせが実際に在るのかを読み取れなくなる。
//!
//! # 実行の欄は、カーネルの側だけがビットになる
//!
//! **x86_64 の変換は、カーネルの側（ユーザーから届かない権限）の `execute` を読む**（2026-10-02。`ADR-0071` の
//! 手順 4）。実行しない権限の葉には、実行禁止のビットが付く。**ユーザーの側の `execute` は、まだ読まない**
//! （ユーザーのページは、今はどれも実行できる。ユーザーの写像に入れるときに、同じ変換が読むようにする）。
//! ここの名前つきの関数が決めた `execute` の値が、そのまま効く。値は手順 4 の決定（`ADR-0071` の決定 1 の 4）に
//! 合わせてある。
//!
//! # 書けて実行もできる権限
//!
//! **ユーザーから届いて、書けて、実行もできる権限は、ページを足す入口が断る**
//! （[`PagePermissions::writable_and_executable`]）。カーネルの側で、書けて実行もできるのは、起動の表の名前
//! （[`PagePermissions::boot_table_unrestricted`]）だけである。
//!
//! # 純粋な論理
//!
//! ポインタにもレジスタにも触らない。ホストの `cargo test` で確かめる。

/// キャッシュしてよいか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    /// メモリ。キャッシュしてよい。
    Cached,
    /// 装置のレジスタやフレームバッファ。キャッシュしない。
    Uncached,
}

/// 1 つのマップに与える権限。**作るのは、下の名前つきの関数だけである**（欄は外へ出していない）。
///
/// # 契約（境界の型。2026-10-02）
///
/// - 共通の側と機械の置き場は、ページを足すときに、用途の名前でこの型を作って渡す。ページテーブルのビットへ直すのは
///   CPU の置き場である。
/// - この型は「何を許すか」だけを持つ。大きさ（4KiB か 2MiB か）や物理の番地は持たない。
/// - `execute` がビットになるのは、今はカーネルの側の権限だけである（モジュールの doc）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagePermissions {
    write: bool,
    execute: bool,
    user: bool,
    cache: Cache,
    shared: bool,
}

impl PagePermissions {
    /// 静的な起動の表の権限。**書けて実行もできる。** **使うのは、起動の表だけである。**
    ///
    /// 起動の表は、カーネルの像と恒等の 1GiB を、区画を分けずに 2MiB の葉で写す。BSP が自前の表へ切り替える
    /// までと、AP がトランポリンを出てから本番の表へ切り替えるまでの、短い間だけ載る（AP のトランポリンは、
    /// 恒等の側で実行する）。**自前の表には、書けて実行もできる権限は 1 つも無い**——カーネルの像は区画ごとの
    /// 名前（[`Self::kernel_code`]・[`Self::kernel_read_only`]・[`Self::kernel_data`]）で写し、恒等と
    /// 直接マッピングは [`Self::kernel_data`] で写す（2026-10-02。以前は `kernel_unrestricted` という名前で、
    /// 自前の表の像と恒等と直接マッピングもこれだった）。
    pub const fn boot_table_unrestricted() -> Self {
        Self {
            write: true,
            execute: true,
            user: false,
            cache: Cache::Cached,
            shared: false,
        }
    }

    /// カーネルのコード（像の `.text`）。読んで実行する。**書けない。**
    pub const fn kernel_code() -> Self {
        Self {
            write: false,
            execute: true,
            user: false,
            cache: Cache::Cached,
            shared: false,
        }
    }

    /// カーネルの、書けるデータ（像の `.data` と `.bss`、CPU ごとのスタック、直接マッピングと恒等の、キャッシュして
    /// よい範囲）。実行はしない。
    pub const fn kernel_data() -> Self {
        Self {
            write: true,
            execute: false,
            user: false,
            cache: Cache::Cached,
            shared: false,
        }
    }

    /// カーネルの、読むだけのデータ（像の読むだけの区画、実行禁止のビットを確かめる試しのページ）。実行はしない。
    pub const fn kernel_read_only() -> Self {
        Self {
            write: false,
            execute: false,
            user: false,
            cache: Cache::Cached,
            shared: false,
        }
    }

    /// 装置のレジスタとフレームバッファ。書けて、キャッシュしない。実行はしない。
    pub const fn kernel_device() -> Self {
        Self {
            write: true,
            execute: false,
            user: false,
            cache: Cache::Uncached,
            shared: false,
        }
    }

    /// ユーザーのプログラムの区画（ELF の `PT_LOAD`）。**書けるか・実行できるかは、区画のフラグに従う。**
    pub const fn user_program(writable: bool, executable: bool) -> Self {
        Self {
            write: writable,
            execute: executable,
            user: true,
            cache: Cache::Cached,
            shared: false,
        }
    }

    /// ユーザーの、書けるデータ（スタック、`brk` で伸ばすヒープ）。実行はしない。
    pub const fn user_data() -> Self {
        Self {
            write: true,
            execute: false,
            user: true,
            cache: Cache::Cached,
            shared: false,
        }
    }

    /// ユーザーへ、ほかの持ち主が居るフレームを見せる（共有メモリと画面の `mmap`。`ADR-0065`）。実行はしない。
    ///
    /// **`shared` の印が付く**——アドレス空間を壊すとき、この葉のフレームはアロケータへ返さない（持ち主が参照の数で
    /// 返す）。
    pub const fn user_shared(writable: bool) -> Self {
        Self {
            write: writable,
            execute: false,
            user: true,
            cache: Cache::Cached,
            shared: true,
        }
    }

    /// 書けるか。
    pub const fn write(self) -> bool {
        self.write
    }

    /// 実行できるか（ビットになるのは、今はカーネルの側の権限だけである）。
    pub const fn execute(self) -> bool {
        self.execute
    }

    /// 書けて、実行もできるか。**ユーザーから届く権限でこれが真のものは、ページを足す入口が断る**
    /// （書けるページは実行できない、という決まり。2026-10-03）。
    pub const fn writable_and_executable(self) -> bool {
        self.write && self.execute
    }

    /// ユーザーから届くか。
    pub const fn user(self) -> bool {
        self.user
    }

    /// キャッシュしてよいか。
    pub const fn cache(self) -> Cache {
        self.cache
    }

    /// ほかの持ち主が居るフレームか（[`Self::user_shared`]）。
    pub const fn shared(self) -> bool {
        self.shared
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **カーネルの側の名前は、どれもユーザーから届かない。** 共有の印も付かない。
    #[test]
    fn the_kernel_side_names_are_never_reachable_from_user_mode() {
        for permissions in [
            PagePermissions::boot_table_unrestricted(),
            PagePermissions::kernel_code(),
            PagePermissions::kernel_data(),
            PagePermissions::kernel_read_only(),
            PagePermissions::kernel_device(),
        ] {
            assert!(!permissions.user(), "{permissions:?}");
            assert!(!permissions.shared(), "{permissions:?}");
        }
    }

    /// **ユーザーの側の名前は、どれもユーザーから届く。** 共有の印が付くのは `user_shared` だけである。
    #[test]
    fn the_user_side_names_are_reachable_and_only_the_shared_one_is_marked() {
        for permissions in [
            PagePermissions::user_program(false, true),
            PagePermissions::user_program(true, false),
            PagePermissions::user_data(),
            PagePermissions::user_shared(true),
            PagePermissions::user_shared(false),
        ] {
            assert!(permissions.user(), "{permissions:?}");
        }
        assert!(PagePermissions::user_shared(true).shared());
        assert!(PagePermissions::user_shared(false).shared());
        assert!(!PagePermissions::user_data().shared());
        assert!(!PagePermissions::user_program(true, true).shared());
    }

    /// **キャッシュしないのは、装置だけである。**
    #[test]
    fn only_the_device_name_is_uncached() {
        assert_eq!(PagePermissions::kernel_device().cache(), Cache::Uncached);
        for permissions in [
            PagePermissions::boot_table_unrestricted(),
            PagePermissions::kernel_data(),
            PagePermissions::kernel_read_only(),
            PagePermissions::user_program(true, true),
            PagePermissions::user_data(),
            PagePermissions::user_shared(true),
        ] {
            assert_eq!(permissions.cache(), Cache::Cached, "{permissions:?}");
        }
    }

    /// **実行できると名乗るのは、起動の表と、カーネルのコードと、実行できる区画だけである。**
    /// 書けるかは、区画のフラグと、読むだけの名前に従う。
    #[test]
    fn write_and_execute_follow_the_names() {
        assert!(PagePermissions::boot_table_unrestricted().execute());
        assert!(PagePermissions::boot_table_unrestricted().write());
        assert!(PagePermissions::kernel_code().execute());
        assert!(!PagePermissions::kernel_code().write());
        assert!(!PagePermissions::kernel_read_only().execute());
        assert!(!PagePermissions::kernel_data().execute());
        assert!(!PagePermissions::kernel_device().execute());
        assert!(!PagePermissions::kernel_read_only().write());
        assert!(!PagePermissions::user_data().execute());
        assert!(PagePermissions::user_data().write());
        assert!(!PagePermissions::user_shared(true).execute());
        assert!(!PagePermissions::user_shared(false).write());
        let text = PagePermissions::user_program(false, true);
        assert!(!text.write() && text.execute());
        let data = PagePermissions::user_program(true, false);
        assert!(data.write() && !data.execute());
    }

    /// **ユーザーの側で、書けて実行もできるのは、区画のフラグの両方が立った形だけである**（入口が断る形）。
    #[test]
    fn only_a_writable_and_executable_segment_is_both_on_the_user_side() {
        assert!(PagePermissions::user_program(true, true).writable_and_executable());
        for permissions in [
            PagePermissions::user_program(false, true),
            PagePermissions::user_program(true, false),
            PagePermissions::user_program(false, false),
            PagePermissions::user_data(),
            PagePermissions::user_shared(true),
            PagePermissions::user_shared(false),
        ] {
            assert!(!permissions.writable_and_executable(), "{permissions:?}");
        }
    }

    /// **カーネルの側で、書けて実行もできるのは、起動の表の名前だけである**（写像ごとの W^X）。
    #[test]
    fn only_the_boot_table_name_is_both_writable_and_executable_on_the_kernel_side() {
        for permissions in [
            PagePermissions::kernel_code(),
            PagePermissions::kernel_data(),
            PagePermissions::kernel_read_only(),
            PagePermissions::kernel_device(),
        ] {
            assert!(
                !(permissions.write() && permissions.execute()),
                "{permissions:?}"
            );
        }
        let boot = PagePermissions::boot_table_unrestricted();
        assert!(boot.write() && boot.execute());
    }
}
