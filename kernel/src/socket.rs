//! unix ドメインのストリームソケット（`ADR-0064`）。**名前で繋ぐ、双方向のバイトの通り道。**
//!
//! # Wayland が要求する形だけを作る
//!
//! **接続はストリームソケットで、端点は名前である**（`docs/wayland-inventory.md`）。
//! **`bind` / `listen` / `accept` / `connect` と、繋がった後の `read` / `write` / `close`。**
//! **fd の運搬（`SCM_RIGHTS`）と `poll` はこの段では作らない。**
//!
//! # パイプの核を再利用する
//!
//! **輪は [`crate::ring::Ring`]（パイプから切り出した）。** **待つ側と起こす側の形は
//! パイプと同じで、違うのは向きが 2 本あることと、端が数ではなく旗であることである**
//! ——**端は各側に 1 つしか無い**（`dup` も fd の運搬も無い）。
//!
//! # 容量
//!
//! **listener は 1 本、接続は同時に 2 つ、輪は向きごとに 1,024 バイトである。**
//! **1,024 の根拠は Wayland の初手である**——**`wl_registry.global` の束（1 つ 40〜60 バイト
//! × 20 前後 ≈ 1 KiB）を、相手が読む前に書き切れる大きさにする。** **256 だと双方が書いて
//! 互いに待つ形が近い。** **書き手が待つ機会は `/data/big`（2,181 バイト）で作る。**
//!
//! # 名前はカーネルの表である
//!
//! **ファイルシステムに inode は作らない**（`ls` に出ず、`unlink` で消えない）。
//! **抽象名（先頭 NUL）は `-EINVAL` である**（Linux 向けのクライアントを動かすことは
//! 目標にしていない。`docs/architecture.md`）。
//!
//! # 起こすのはここである
//!
//! **パイプと同じ規律である**——**閉じる経路は `close` からも表の `Drop` からも来るので、
//! 起こす場所を 1 つにする。** **`connect` は accept で待つ者を、書きは読み手を、読みは
//! 書き手を、端を閉じたら相手側の読み手と書き手を起こす。**

use core::sync::atomic::{AtomicU64, Ordering};

use common::critical::Locked;

use crate::ring::Ring;
use crate::task::{self, Wait};

/// 輪の大きさ（バイト。向きごと）。**根拠はモジュールの doc にある。**
pub const SOCKET_RING: usize = 1024;

/// 同時に在れる listener の数。
pub const MAX_LISTENERS: usize = 1;

/// 同時に在れる接続の数（accept 前の待ち行列を含む）。
pub const MAX_CONNECTIONS: usize = 2;

/// 名前の最大長（バイト。NUL を含まない）。
pub const NAME_MAX: usize = 31;

/// 接続のどちら側か。**輪の添字にも使う**——`rings[side]` は「その側へ流れるバイト」である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `connect` した側。
    Client = 0,
    /// `accept` した側。
    Server = 1,
}

impl Side {
    /// 相手側。
    pub fn peer(self) -> Self {
        match self {
            Self::Client => Self::Server,
            Self::Server => Self::Client,
        }
    }
}

/// 名前を持つ待ち受け。
struct Listener {
    in_use: bool,
    /// `listen` 済みか。**`bind` だけでは繋げない**（Linux と同じ）。
    listening: bool,
    name: [u8; NAME_MAX],
    name_len: u8,
}

impl Listener {
    const EMPTY: Self = Self {
        in_use: false,
        listening: false,
        name: [0; NAME_MAX],
        name_len: 0,
    };

    fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

/// 接続 1 つの状態。
struct Connection {
    in_use: bool,
    /// どの listener に繋いだか。
    listener: u8,
    /// `accept` で server 側が fd になったか。**なる前は listener が server 側を持つ。**
    accepted: bool,
    /// 各側の端が開いているか（`[client, server]`）。
    open: [bool; 2],
    /// 各側へ流れる輪（`[client へ, server へ]`）。
    rings: [Ring<SOCKET_RING>; 2],
}

impl Connection {
    const EMPTY: Self = Self {
        in_use: false,
        listener: 0,
        accepted: false,
        open: [false, false],
        rings: [Ring::EMPTY, Ring::EMPTY],
    };

    fn is_open(&self, side: Side) -> bool {
        self.open[side as usize]
    }
}

static LISTENERS: [Locked<Listener>; MAX_LISTENERS] = [Locked::new(Listener::EMPTY)];
static CONNECTIONS: [Locked<Connection>; MAX_CONNECTIONS] = [
    Locked::new(Connection::EMPTY),
    Locked::new(Connection::EMPTY),
];

/// `bind` した回数（計器）。
static BOUND: AtomicU64 = AtomicU64::new(0);
/// 名前が取られていて `bind` を断った回数（計器。`-EADDRINUSE`）。
static BIND_REFUSED: AtomicU64 = AtomicU64::new(0);
/// listener を閉じて名前を返した回数（計器）。
static LISTENERS_RELEASED: AtomicU64 = AtomicU64::new(0);
/// 作った接続の数（計器）。
static CONNECTIONS_CREATED: AtomicU64 = AtomicU64::new(0);
/// 同時に在った接続の最大（計器）。
static CONNECTIONS_AT_ONCE_MAX: AtomicU64 = AtomicU64::new(0);
/// 両端が閉じて枠を返した回数（計器）。
static CONNECTIONS_RELEASED: AtomicU64 = AtomicU64::new(0);
/// 名前の listener が無くて `connect` を断った回数（計器。`-ECONNREFUSED`）。
static CONNECT_REFUSED: AtomicU64 = AtomicU64::new(0);
/// 待ち行列が満杯で `connect` を断った回数（計器。`-EAGAIN`）。
static CONNECT_BACKLOG_FULL: AtomicU64 = AtomicU64::new(0);
/// `accept` が待った回数（計器）。
static ACCEPT_WAITS: AtomicU64 = AtomicU64::new(0);
/// 読み手が空で待った回数（計器）。
static READER_WAITS: AtomicU64 = AtomicU64::new(0);
/// 書きが起こした読み手の数（計器）。**閉じの起こしの肩代わりを見分ける**（`pipe.rs` と同じ）。
static READERS_WOKEN_BY_WRITE: AtomicU64 = AtomicU64::new(0);
/// 書き手が満杯で待った回数（計器）。
static WRITER_WAITS: AtomicU64 = AtomicU64::new(0);
/// 読みが起こした書き手の数（計器）。
static WRITERS_WOKEN_BY_READ: AtomicU64 = AtomicU64::new(0);
/// 相手側が閉じているのに書こうとした回数（計器。`-EPIPE`）。
static EPIPE_SEEN: AtomicU64 = AtomicU64::new(0);
/// 相手側が閉じて EOF を返した回数（計器）。
static EOF_SEEN: AtomicU64 = AtomicU64::new(0);

macro_rules! gauge {
    ($name:ident, $static:ident) => {
        pub fn $name() -> u64 {
            $static.load(Ordering::Relaxed)
        }
    };
}

gauge!(bound, BOUND);
gauge!(bind_refused, BIND_REFUSED);
gauge!(listeners_released, LISTENERS_RELEASED);
gauge!(connections_created, CONNECTIONS_CREATED);
gauge!(connections_at_once_max, CONNECTIONS_AT_ONCE_MAX);
gauge!(connections_released, CONNECTIONS_RELEASED);
gauge!(connect_refused, CONNECT_REFUSED);
gauge!(connect_backlog_full, CONNECT_BACKLOG_FULL);
gauge!(accept_waits, ACCEPT_WAITS);
gauge!(reader_waits, READER_WAITS);
gauge!(readers_woken_by_write, READERS_WOKEN_BY_WRITE);
gauge!(writer_waits, WRITER_WAITS);
gauge!(writers_woken_by_read, WRITERS_WOKEN_BY_READ);
gauge!(epipe_seen, EPIPE_SEEN);
gauge!(eof_seen, EOF_SEEN);

/// `accept` が待つことを数える。**待つのは呼び出し側である**（`syscall`）。
pub fn note_accept_wait() {
    ACCEPT_WAITS.fetch_add(1, Ordering::Relaxed);
}

/// 読み手が空で待つことを数える。
pub fn note_reader_wait() {
    READER_WAITS.fetch_add(1, Ordering::Relaxed);
}

/// 書き手が満杯で待つことを数える。
pub fn note_writer_wait() {
    WRITER_WAITS.fetch_add(1, Ordering::Relaxed);
}

/// `bind` の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    /// その名前は取られている（`-EADDRINUSE`）。
    NameTaken,
    /// listener の枠が無い（`-ENOBUFS`）。
    NoRoom,
}

/// 名前を取る。**`listen` はまだである。** **名前の検証（空・長さ・先頭 NUL）は呼び出し側が
/// 済ませている。**
///
/// 破壊 (`ADR-0064`, socket-bind-ignores-taken-name): 取られている名前を見ない。
/// **同じ名前の 2 本目が `-EADDRINUSE` にならない。**
pub fn bind(name: &[u8]) -> Result<u8, BindError> {
    debug_assert!(!name.is_empty() && name.len() <= NAME_MAX);
    #[cfg(not(feature = "socket-bind-ignores-taken-name"))]
    if LISTENERS.iter().any(|slot| {
        let listener = slot.lock();
        listener.in_use && listener.name() == name
    }) {
        BIND_REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(BindError::NameTaken);
    }
    for (index, slot) in LISTENERS.iter().enumerate() {
        let mut listener = slot.lock();
        if listener.in_use {
            continue;
        }
        *listener = Listener::EMPTY;
        listener.in_use = true;
        listener.name[..name.len()].copy_from_slice(name);
        listener.name_len = name.len() as u8;
        BOUND.fetch_add(1, Ordering::Relaxed);
        return Ok(index as u8);
    }
    Err(BindError::NoRoom)
}

/// 待ち受けを始める。**`bind` 済みの listener だけを受ける**（範囲外なら偽）。
pub fn listen(listener: u8) -> bool {
    let Some(slot) = LISTENERS.get(listener as usize) else {
        return false;
    };
    let mut state = slot.lock();
    if !state.in_use {
        return false;
    }
    state.listening = true;
    true
}

/// `connect` の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectError {
    /// その名前で待ち受けている者が居ない（`-ECONNREFUSED`）。
    NoListener,
    /// 接続の枠が無い（`-EAGAIN`）。
    NoRoom,
}

/// 名前で繋ぐ。**接続を 1 つ作り、client 側の添字を返す。** **accept で待つ者を起こす。**
///
/// 破壊 (`ADR-0064`, socket-connect-ignores-name): 名前を見ずに、待ち受けている最初の
/// listener へ繋ぐ。**無い名前への `connect` が `-ECONNREFUSED` にならない。**
pub fn connect(name: &[u8]) -> Result<u8, ConnectError> {
    let listener = LISTENERS.iter().position(|slot| {
        let listener = slot.lock();
        #[cfg(not(feature = "socket-connect-ignores-name"))]
        let name_matches = listener.name() == name;
        #[cfg(feature = "socket-connect-ignores-name")]
        let name_matches = true;
        listener.in_use && listener.listening && name_matches
    });
    let Some(listener) = listener else {
        CONNECT_REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(ConnectError::NoListener);
    };
    let mut created = None;
    for (index, slot) in CONNECTIONS.iter().enumerate() {
        let mut connection = slot.lock();
        if connection.in_use {
            continue;
        }
        *connection = Connection::EMPTY;
        connection.in_use = true;
        connection.listener = listener as u8;
        connection.open = [true, true];
        created = Some(index as u8);
        break;
    }
    let Some(index) = created else {
        CONNECT_BACKLOG_FULL.fetch_add(1, Ordering::Relaxed);
        return Err(ConnectError::NoRoom);
    };
    CONNECTIONS_CREATED.fetch_add(1, Ordering::Relaxed);
    let at_once = CONNECTIONS.iter().filter(|slot| slot.lock().in_use).count() as u64;
    CONNECTIONS_AT_ONCE_MAX.fetch_max(at_once, Ordering::Relaxed);
    task::wake_tasks_waiting_on(Wait::SocketAcceptable {
        listener: listener as u8,
    });
    Ok(index)
}

/// `accept` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// 繋がった接続の添字（server 側として使う）。
    Connection(u8),
    /// 待っている接続が無い。**待つこと。**
    Empty,
    /// その listener は無い。
    NoListener,
}

/// 待ち行列の先頭を取る。**取れなければ [`AcceptOutcome::Empty`]**——**待つのは呼び出し側である。**
pub fn accept(listener: u8) -> AcceptOutcome {
    let Some(slot) = LISTENERS.get(listener as usize) else {
        return AcceptOutcome::NoListener;
    };
    {
        let state = slot.lock();
        if !state.in_use || !state.listening {
            return AcceptOutcome::NoListener;
        }
    }
    for (index, slot) in CONNECTIONS.iter().enumerate() {
        let mut connection = slot.lock();
        if connection.in_use && connection.listener == listener && !connection.accepted {
            connection.accepted = true;
            return AcceptOutcome::Connection(index as u8);
        }
    }
    AcceptOutcome::Empty
}

/// 読んだ結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// これだけ取れた（1 以上）。
    Bytes(usize),
    /// 空で、相手側はもう閉じている（EOF）。
    Eof,
    /// 空だが、相手側はまだ開いている。**待つこと。**
    Empty,
    /// その接続は無い。
    NoConnection,
}

/// `side` へ流れてきたバイトを `dst` へ移す。**取れたら相手側の書き手を起こす**（空きができた）。
///
/// 破壊 (`ADR-0064`, socket-read-empty-returns-zero): 空を EOF と誤る。**返事が届く前に
/// クライアントが終わる。**
pub fn read_into(conn: u8, side: Side, dst: &mut [u8]) -> ReadOutcome {
    let Some(slot) = CONNECTIONS.get(conn as usize) else {
        return ReadOutcome::NoConnection;
    };
    let outcome = {
        let mut state = slot.lock();
        if !state.in_use {
            ReadOutcome::NoConnection
        } else if state.rings[side as usize].is_empty() {
            #[cfg(feature = "socket-read-empty-returns-zero")]
            {
                ReadOutcome::Eof
            }
            #[cfg(not(feature = "socket-read-empty-returns-zero"))]
            if state.is_open(side.peer()) {
                ReadOutcome::Empty
            } else {
                ReadOutcome::Eof
            }
        } else {
            ReadOutcome::Bytes(state.rings[side as usize].take(dst))
        }
    };
    match outcome {
        ReadOutcome::Bytes(_) => {
            let woken = task::wake_tasks_waiting_on(Wait::SocketWritable {
                conn,
                side: side.peer(),
            });
            WRITERS_WOKEN_BY_READ.fetch_add(woken as u64, Ordering::Relaxed);
        }
        ReadOutcome::Eof => {
            EOF_SEEN.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    outcome
}

/// 書いた結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// これだけ入った（1 以上。**部分書きである**——`userlib::write_all` が回す）。
    Bytes(usize),
    /// 1 バイトも入らない。**待つこと。**
    Full,
    /// 相手側が閉じている（`-EPIPE`）。
    PeerClosed,
    /// その接続は無い。
    NoConnection,
}

/// `side` から相手側へ `src` を入るだけ入れる。**入ったら相手側の読み手を起こす。**
///
/// 破壊 (`ADR-0064`, socket-write-ignores-peer-closed): 相手側が閉じているのを見ない。
/// **閉じた相手への書きが `-EPIPE` にならず、輪へ入って数を返す。**
///
/// 破壊 (`ADR-0064`, socket-write-does-not-wake-reader): 起こさない。**返事を待つ側と
/// 要求を待つ側が両方待ち、出力が伸びなくなる。**
pub fn write_from(conn: u8, side: Side, src: &[u8]) -> WriteOutcome {
    let Some(slot) = CONNECTIONS.get(conn as usize) else {
        return WriteOutcome::NoConnection;
    };
    let outcome = {
        let mut state = slot.lock();
        #[cfg(not(feature = "socket-write-ignores-peer-closed"))]
        let peer_closed = !state.is_open(side.peer());
        #[cfg(feature = "socket-write-ignores-peer-closed")]
        let peer_closed = false;
        if !state.in_use {
            WriteOutcome::NoConnection
        } else if peer_closed {
            EPIPE_SEEN.fetch_add(1, Ordering::Relaxed);
            WriteOutcome::PeerClosed
        } else {
            let put = state.rings[side.peer() as usize].put(src);
            if put == 0 {
                WriteOutcome::Full
            } else {
                WriteOutcome::Bytes(put)
            }
        }
    };
    #[cfg(not(feature = "socket-write-does-not-wake-reader"))]
    if matches!(outcome, WriteOutcome::Bytes(_)) {
        let woken = task::wake_tasks_waiting_on(Wait::SocketReadable {
            conn,
            side: side.peer(),
        });
        READERS_WOKEN_BY_WRITE.fetch_add(woken as u64, Ordering::Relaxed);
    }
    outcome
}

/// 端を 1 つ閉じる。**相手側の読み手（EOF を見に行かせる）と書き手（`-EPIPE` を見に行かせる）を
/// 起こす。** **両端が閉じたら枠を返す。**
///
/// 破壊 (`ADR-0064`, socket-close-keeps-peer-open): 端を閉じたことにしない。**相手側に
/// EOF が来ず、サーバーが永久に待つ。**
///
/// 破壊 (`ADR-0064`, socket-release-keeps-slot): 両端が閉じても枠を返さない。**3 つ目の
/// 接続が `-EAGAIN` になる。**
pub fn close_end(conn: u8, side: Side) {
    let Some(slot) = CONNECTIONS.get(conn as usize) else {
        return;
    };
    let closed = {
        let mut state = slot.lock();
        if !state.in_use || !state.is_open(side) {
            false
        } else {
            #[cfg(not(feature = "socket-close-keeps-peer-open"))]
            {
                state.open[side as usize] = false;
            }
            #[cfg(not(feature = "socket-release-keeps-slot"))]
            if !state.open[0] && !state.open[1] {
                *state = Connection::EMPTY;
                CONNECTIONS_RELEASED.fetch_add(1, Ordering::Relaxed);
            }
            true
        }
    };
    if closed {
        task::wake_tasks_waiting_on(Wait::SocketReadable {
            conn,
            side: side.peer(),
        });
        task::wake_tasks_waiting_on(Wait::SocketWritable {
            conn,
            side: side.peer(),
        });
    }
}

/// listener を閉じる。**名前を返し、accept 前の接続の server 側を閉じる**（client 側は
/// EOF と `-EPIPE` を見る）。
pub fn close_listener(listener: u8) {
    let Some(slot) = LISTENERS.get(listener as usize) else {
        return;
    };
    let released = {
        let mut state = slot.lock();
        if !state.in_use {
            false
        } else {
            *state = Listener::EMPTY;
            true
        }
    };
    if !released {
        return;
    }
    LISTENERS_RELEASED.fetch_add(1, Ordering::Relaxed);
    let pending: [bool; MAX_CONNECTIONS] = core::array::from_fn(|index| {
        let connection = CONNECTIONS[index].lock();
        connection.in_use && connection.listener == listener && !connection.accepted
    });
    for (index, is_pending) in pending.iter().enumerate() {
        if *is_pending {
            close_end(index as u8, Side::Server);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // **`Locked` と起こしはホストでは動かないので、状態の規則だけを見る。**

    #[test]
    fn the_side_and_its_peer_index_the_two_rings() {
        assert_eq!(Side::Client as usize, 0);
        assert_eq!(Side::Server as usize, 1);
        assert_eq!(Side::Client.peer(), Side::Server);
        assert_eq!(Side::Server.peer(), Side::Client);
    }

    #[test]
    fn a_connection_starts_open_on_both_sides_and_is_not_accepted() {
        let mut connection = Connection::EMPTY;
        assert!(!connection.in_use);
        connection.in_use = true;
        connection.open = [true, true];
        assert!(connection.is_open(Side::Client) && connection.is_open(Side::Server));
        assert!(!connection.accepted);
        assert!(connection.rings[0].is_empty() && connection.rings[1].is_empty());
    }

    #[test]
    fn the_listener_name_is_the_recorded_length() {
        let mut listener = Listener::EMPTY;
        listener.name[..9].copy_from_slice(b"wayland-0");
        listener.name_len = 9;
        assert_eq!(listener.name(), b"wayland-0");
    }
}
