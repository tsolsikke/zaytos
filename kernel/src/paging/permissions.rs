//! ページの権限（2026-10-02。`ADR-0071` の手順 3）。**CPU に依らない言い方で持つ。**
//!
//! 「書けるか・実行できるか・ユーザーから届くか・キャッシュするか」を 1 つの型にし、ページテーブルのビットへ直すのは
//! CPU の置き場の 1 つの関数にする（x86_64 では `crate::arch::x86_64` の `paging::entry` の `leaf_flags` と
//! `table_flags`）。**権限を決める所は、ここの名前つきの関数だけにする。** 欄を直に書かせると、ページを足す所ごとに
//! 組み合わせが散らばり、どの組み合わせが実際に在るのかを読み取れなくなる。
//!
//! # 実行の欄は、まだビットにならない
//!
//! **`execute` は型に入っているが、x86_64 の変換は手順 4 までこの欄を読まない**（今は、どのページも実行できる）。
//! 手順 3 は「いまの権限を保つ」段なので、ページテーブルの項目は 1 つも変えない。手順 4 で変えるのは、変換が
//! `execute` を読むようにすることと、CPU で実行禁止を有効にすることである。**そのとき、ここの名前つきの関数が
//! 決めた `execute` の値が、そのまま効く。** 値は手順 4 の決定（`ADR-0071` の決定 1 の 4）に合わせてある。
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
/// - `execute` は、手順 4 まではビットにならない（モジュールの doc）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagePermissions {
    write: bool,
    execute: bool,
    user: bool,
    cache: Cache,
    shared: bool,
}

impl PagePermissions {
    /// カーネルの、書けて実行もできるメモリ。**区画ごとの権限を持たない今のカーネルの像と、起動の表、恒等、
    /// 直接マッピングの、キャッシュしてよい範囲がこれである。**
    ///
    /// **手順 4 で、この名前は分かれる。** カーネルの像は、区画ごとの名前（コード＝読んで実行する、読むだけの
    /// データ、書けるデータ）に分け、直接マッピングは実行しない名前へ移す。そのときに、この関数を使う所を
    /// 1 つずつ見直す。
    pub const fn kernel_unrestricted() -> Self {
        Self {
            write: true,
            execute: true,
            user: false,
            cache: Cache::Cached,
            shared: false,
        }
    }

    /// カーネルの、書けるデータ（CPU ごとのスタックなど）。実行はしない。
    pub const fn kernel_data() -> Self {
        Self {
            write: true,
            execute: false,
            user: false,
            cache: Cache::Cached,
            shared: false,
        }
    }

    /// カーネルの、読むだけのデータ。**今は、起動時の Ring 3 の試しの破壊テストだけが使う**（ユーザーに見せるはずの
    /// 読み取り専用のページを、カーネル専用でマップする形）。
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

    /// 実行できるか（手順 4 までは、ビットにならない）。
    pub const fn execute(self) -> bool {
        self.execute
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
            PagePermissions::kernel_unrestricted(),
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
            PagePermissions::kernel_unrestricted(),
            PagePermissions::kernel_data(),
            PagePermissions::kernel_read_only(),
            PagePermissions::user_program(true, true),
            PagePermissions::user_data(),
            PagePermissions::user_shared(true),
        ] {
            assert_eq!(permissions.cache(), Cache::Cached, "{permissions:?}");
        }
    }

    /// **実行できると名乗るのは、今のカーネルの像と、実行できる区画だけである**（手順 4 で効く値）。
    /// 書けるかは、区画のフラグと、読むだけの名前に従う。
    #[test]
    fn write_and_execute_follow_the_names() {
        assert!(PagePermissions::kernel_unrestricted().execute());
        assert!(PagePermissions::kernel_unrestricted().write());
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
}
