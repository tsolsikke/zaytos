//! スキャンコード（セット 1）から文字への変換。
//!
//! **純粋ロジック。** ポート I/O を一切含まず、ホスト `cargo test` で検証する。
//! 実機で文字が化けたとき、ここが正しいと分かっていれば原因をハードウェア側
//! （i8042 の設定、割り込み、リングバッファ）に絞り込める。
//!
//! ## 対応範囲
//!
//! **JIS 配列（106/109）の英数字と記号。** 具体的には、数字段・英字 3 段・
//! スペース・Enter・Tab・Backspace・Esc・矢印と、それらの Shift 記号である。
//!
//! **既定を JIS にしたのは運用者の決定である**（zi-e）。**運用者は日本語
//! キーボードを使っており、US の表では `:` が Shift 無しで打てなかった**
//! ——`zi` のコマンド行は `:` から始まるので、毎回当たっていた。
//!
//! **配列は選べない。1 つに固定する。** **選ぶ手段そのものが無いためである**
//! ——環境変数もカーネルコマンドラインも無い（`docs/deferred-decisions.md` に
//! 行がある）。**2 人目の利用者が来たときに、そこから考え直すこと。**
//!
//! 対応しないもの（受けても状態機械は壊れず、[`KeyEvent::Unsupported`] として
//! 報告する）:
//!
//! - ファンクションキー、テンキー
//! - Ctrl / Alt との組み合わせ（Ctrl+C だけは例外。下記）
//! - JIS 以外の配列
//!
//! ## ASCII の記号は全部打てる
//!
//! **JIS でも、印字可能な ASCII の記号 32 個すべてに経路がある**（単体テストが
//! 表から数えて主張する）。**そのために、US には無い 2 つのキーを扱う。**
//! [`SCANCODE_JIS_RO`]（`\` と `_`）と [`SCANCODE_JIS_YEN`]（`\` と `|`）で、
//! **どちらも `0x40` 以上に居るので変換表の外である。**
//!
//! **この 2 つを落とすと、`\` と `_` と `|` が打てなくなる。** US では
//! `\`（`0x2B`）1 つで足りていたが、**JIS はそこが `]` になっている。**
//!
//! ### 表が正しいことと、打鍵が届くことは別である
//!
//! **単体テストが固定するのは表と [`Decoder::character_for`] の分岐までである。**
//! **`0x40` の外に居る 2 つは、範囲の判定より先に引く経路が要る**ので、
//! **その経路が繋がっているかは、実機の消費者まで通さないと言えない。**
//!
//! **消費者は 2 つあり、判定も 2 つ置いてある**（`xtask`）——
//! `--interrupt-test keyboard` がカーネル側（`crate::interrupts` の
//! `drain_keyboard`）を、`--shell-test` が Ring 3 の前景経路を見る。
//! **前景が取られている間、前者は 1 バイトも取り出さない**ので、
//! **片方が緑でも、もう片方は何も言っていない。**
//!
//! ## `¥` キーは `\` を出す
//!
//! **`¥`（U+00A5）そのものは出さない。** 理由は 2 つある。**前景へ渡すのは
//! バイトで、いまの経路は ASCII しか通らない**（`crate::input` の
//! `bytes_for_event`）。**そして、このキーの Shift 側の刻印は `|` で、すでに
//! ASCII である**——素の側だけを非 ASCII にすると、1 つのキーの 2 つの刻印が
//! 別の世界の字になる。
//!
//! **歴史的にも同じ位置である**（Shift-JIS は `0x5C` に `¥` を置いた）。
//!
//! ## JIS 固有キーは無視する。**黙って落とさない**
//!
//! **変換（`0x79`）・無変換（`0x7B`）・かな（`0x70`）・半角/全角（`0x29`）は
//! 文字を持たない。** [`KeyEvent::Unsupported`] として報告する——
//! **無視すると決めたのであって、取りこぼしているのではない。**
//!
//! **かな入力も IME も無い。** 日本語を打つ道が無いので、**この 4 つは
//! その道ができるまで意味を持たない。** **半角/全角（`0x29`）は US では
//! `` ` `` の位置だが、JIS の `` ` `` は Shift+`@` にある**ので、失う字は無い。
//!
//! ## セット 1 を前提にしてよい理由
//!
//! キーボード自身が出すのはセット 2 だが、i8042 のコンフィグバイト
//! bit6（translation）が立っていればコントローラがセット 1 へ翻訳する。
//! ZaytOS は起動時にこのビットを**実際に読んで確認**し、落ちていれば立てて
//! 読み直す。したがってここはセット 1 だけを扱えばよい。両方に対応すると
//! 分岐が倍になり、しかも片方は実機で試せない。
//!
//! ## Caps Lock の LED は更新しない
//!
//! LED の点灯にはキーボードへのコマンド送信（0xED + ビットマスク）と ACK の
//! 待ち合わせが必要で、i8042 への出力方向の手順が新たに要る。M4-e の主題は
//! 割り込みであり、複雑度に見合わないため見送る
//! （`docs/deferred-decisions.md`）。**内部状態としての Caps Lock は正しく
//! 管理する**ので、変換結果は LED の有無に関わらず正しい。

/// メインループが受け取るキーイベント。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    /// 表示すべき文字。
    Char(char),
    /// Enter。改行として扱う。
    Enter,
    /// Backspace。
    ///
    /// **M4-e では画面上の消去を行わない。** 消去にはコンソール側でセルごとの
    /// 占有種別（全角の先頭 / 後続）を管理する必要があり、割り込みとは別の
    /// 仕事になる（`docs/deferred-decisions.md`）。キーとしては認識する。
    Backspace,
    /// カーソル左（S12 前の手当て）。
    ///
    /// # なぜ `Unsupported` から出すのか
    ///
    /// **`Unsupported` は「スキャンコードは分かるが扱わない」の集合である。**
    /// 扱うようになったものを残すと、**呼び出し側が数えている「扱えなかった数」に
    /// 扱えたものが混ざる。** 種を分ける。
    ArrowLeft,
    /// カーソル右（S12 前の手当て）。
    ArrowRight,
    /// カーソル上（zi-a）。
    ///
    /// **左右を入れたとき「上下は分けない。使う者がいない機構は検算が置けない」と
    /// 書いた。** 使う者（`zi`。vision から段になった）が決まったので分けた。
    ArrowUp,
    /// Delete（zi-f）。**カーソル位置の字を消す。**
    ///
    /// # なぜ種を足すのか
    ///
    /// **Backspace と別のキーである。** **Backspace は前を消し、
    /// Delete はその場を消す。** **同じ種にすると、消す向きを
    /// 受け手が決められない。**
    Delete,
    /// カーソル下（zi-a）。
    ArrowDown,
    /// Esc（zi-a）。**`zi` のノーマルモードへ戻るキーである。**
    ///
    /// **`Char('\x1b')` にはしない。** 表示すべき文字ではない——
    /// `Char` の集合は「画面へ出るもの」で、Esc はキーそのものである
    /// （矢印と同じ判断）。バイトへの落とし方は `input.rs` が持つ。
    Escape,
    /// 対応していないスキャンコード。**無言で捨てず、呼び出し側が数える。**
    ///
    /// 押下（make）のときだけ報告する。離した（break）ときは報告しない。
    Unsupported(u8),
}

/// ブレークコード（キーを離した）を示すビット。
///
/// セット 1 では、押下コードに 0x80 を立てたものが離したときのコードになる。
const BREAK_BIT: u8 = 0x80;

/// 拡張スキャンコードのプレフィックス。次の 1 バイトと組で 1 つのキーを表す。
pub(crate) const EXTENDED_PREFIX: u8 = 0xE0;

/// Pause / Break のプレフィックス。
///
/// **`0xE0` と同じ扱いにしてはならない。** Pause は
/// `E1 1D 45 E1 9D C5` の 6 バイト列で、**内部に 2 個目の `E1` を含む**。
/// 「次の 1 バイトを飛ばす」実装だと内側の `E1` で状態機械が入れ子になり、
/// 以降のバイト境界がずれて**キー入力全体が壊れる**。しかも Pause を
/// 押さなければ再現しないため、気づくのが遅れる。
pub(crate) const PAUSE_PREFIX: u8 = 0xE1;

/// `PAUSE_PREFIX` の後に続き、無条件に読み捨てるバイト数。
///
/// 内側の `E1` も「数のうち」として飲み込むので、入れ子にならない。
pub(crate) const PAUSE_TRAILING_BYTES: u8 = 5;

// 修飾キーのスキャンコード（押下側）。
const SCANCODE_LEFT_SHIFT: u8 = 0x2A;
const SCANCODE_RIGHT_SHIFT: u8 = 0x36;
const SCANCODE_CAPS_LOCK: u8 = 0x3A;

/// Ctrl のスキャンコード（S12 前の手当て、C）。
///
/// **左右で同じ値である。** 右 Ctrl は `0xE0 0x1D` で、接頭辞が付くだけである。
/// **接頭辞の有無を区別しない**ので、左右どちらでも Ctrl として効く。
pub(crate) const SCANCODE_CTRL: u8 = 0x1D;

/// `C` のスキャンコード（S12 前の手当て、C）。
pub(crate) const SCANCODE_C: u8 = 0x2E;

/// Ctrl+C が作る制御文字。**ASCII の ETX（`0x03`）である。**
pub(crate) const CTRL_C_BYTE: u8 = 0x03;

// 文字ではないが意味を持つキー。
const SCANCODE_BACKSPACE: u8 = 0x0E;

/// 拡張コードのカーソル左（`0xE0 0x4B`）。
const SCANCODE_ARROW_LEFT: u8 = 0x4B;
/// 拡張コードのカーソル右（`0xE0 0x4D`）。
const SCANCODE_ARROW_RIGHT: u8 = 0x4D;
/// 拡張コードのカーソル上（`0xE0 0x48`。zi-a）。
///
/// **素の `0x48` はキーパッドの 8 である**（表で `'\0'`、`Unsupported` に落ちる）。
/// 矢印は必ず `0xE0` 付きで届くので、取り違えは起きない。下も同じ。
const SCANCODE_ARROW_UP: u8 = 0x48;
/// 拡張コードのカーソル下（`0xE0 0x50`。zi-a）。
const SCANCODE_ARROW_DOWN: u8 = 0x50;
/// 拡張コードの Delete（`0xE0 0x53`。zi-f）。
///
/// **素の `0x53` はキーパッドの `.` である**（表で `'\0'`、`Unsupported` に
/// 落ちる）。**Delete は必ず `0xE0` 付きで届くので、取り違えは起きない**
/// （矢印と同じ形である）。
const SCANCODE_DELETE: u8 = 0x53;
/// Esc（`0x01`。zi-a）。
const SCANCODE_ESC: u8 = 0x01;
const SCANCODE_TAB: u8 = 0x0F;
const SCANCODE_ENTER: u8 = 0x1C;

/// 変換表の大きさ。これ以上のコードは未対応として扱う。
///
/// # JIS 固有の 2 つはこの外に居る
///
/// **`0x73`（ろ）と `0x7D`（¥）は `0x40` を越えている。** 表を `0x80` まで
/// 伸ばすと、**`0x40` から `0x72` までの 51 個が空欄で埋まる**——表の見た目が
/// 「何も無い区間」に占められる。**2 つだけなので名前で持つ**
/// （[`JIS_ONLY_KEYS`]）。
const TABLE_LEN: usize = 0x40;

/// JIS 固有キー——`\` と `_` の刻印を持つ（**ろ**。右 Shift の左）。
const SCANCODE_JIS_RO: u8 = 0x73;

/// JIS 固有キー——`¥` と `|` の刻印を持つ（Backspace の左）。
///
/// **素の側は `\` を出す**（モジュール doc の「`¥` キーは `\` を出す」）。
const SCANCODE_JIS_YEN: u8 = 0x7D;

/// 変換表の外に居る JIS 固有キー。`(スキャンコード, 素, Shift)`。
///
/// **記号なので Shift だけが効く**（表の側と同じ規則。Caps Lock は関係しない）。
///
/// 破壊 (zi-e, keyboard-us-layout-test): **空にする。**
/// **US の表には対応するキーが無いので、無いことが US の表そのものである。**
#[cfg(not(feature = "keyboard-us-layout-test"))]
const JIS_ONLY_KEYS: &[(u8, char, char)] =
    &[(SCANCODE_JIS_RO, '\\', '_'), (SCANCODE_JIS_YEN, '\\', '|')];

#[cfg(feature = "keyboard-us-layout-test")]
const JIS_ONLY_KEYS: &[(u8, char, char)] = &[];

/// Shift を押していないときの文字。JIS 配列。`'\0'` は「文字ではない」。
///
/// # `0x29` は半角/全角である
///
/// **US ではここが `` ` `` だが、JIS は変換の切り替えキーである。**
/// 文字を持たないので `'\0'` を置く。**`` ` `` は Shift+`@`（`0x1A`）にある。**
#[cfg(not(feature = "keyboard-us-layout-test"))]
const UNSHIFTED: [char; TABLE_LEN] = [
    '\0', '\0', '1', '2', '3', '4', '5', '6', // 0x00-0x07
    '7', '8', '9', '0', '-', '^', '\0', '\0', // 0x08-0x0F (0x0E=BS, 0x0F=Tab)
    'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', // 0x10-0x17
    'o', 'p', '@', '[', '\0', '\0', 'a', 's', // 0x18-0x1F (0x1C=Enter, 0x1D=LCtrl)
    'd', 'f', 'g', 'h', 'j', 'k', 'l', ';', // 0x20-0x27
    ':', '\0', '\0', ']', 'z', 'x', 'c', 'v', // 0x28-0x2F (0x29=半角/全角, 0x2A=LShift)
    'b', 'n', 'm', ',', '.', '/', '\0', '\0', // 0x30-0x37 (0x36=RShift, 0x37=keypad *)
    '\0', ' ', '\0', '\0', '\0', '\0', '\0', '\0', // 0x38-0x3F (0x39=Space, 0x3A=Caps)
];

/// Shift を押しているときの文字。JIS 配列。
///
/// # `0x0B`（`0`）だけ、Shift 側に字が無い
///
/// **JIS の `0` キーには Shift の刻印が無い**（刻印は `0` と かなの「わ」で、
/// ASCII の記号を持たない）。**US はここが `)` だが、JIS の `)` は Shift+`9`
/// にある**ので、失う字は無い。
///
/// **`'\0'` を置くので、Shift+`0` は [`KeyEvent::Unsupported`] になる。**
/// **2 つの表が食い違う唯一の位置であり、単体テストがそう主張している。**
#[cfg(not(feature = "keyboard-us-layout-test"))]
const SHIFTED: [char; TABLE_LEN] = [
    '\0', '\0', '!', '"', '#', '$', '%', '&', // 0x00-0x07
    '\'', '(', ')', '\0', '=', '~', '\0', '\0', // 0x08-0x0F (0x0B=Shift+0 は無い)
    'Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', // 0x10-0x17
    'O', 'P', '`', '{', '\0', '\0', 'A', 'S', // 0x18-0x1F
    'D', 'F', 'G', 'H', 'J', 'K', 'L', '+', // 0x20-0x27
    '*', '\0', '\0', '}', 'Z', 'X', 'C', 'V', // 0x28-0x2F
    'B', 'N', 'M', '<', '>', '?', '\0', '\0', // 0x30-0x37
    '\0', ' ', '\0', '\0', '\0', '\0', '\0', '\0', // 0x38-0x3F
];

// 破壊 (zi-e, keyboard-us-layout-test): **US の表のまま返す。**
//
// **既定を JIS にした段の判定が、実際に表を見ていることを確かめる。**
// **記号の十数個と、JIS 固有の 2 キーが同時に US へ戻る**ので、
// **ホストの単体テストが 6 本落ちる**（実測）——差の一覧・`:` の位置・
// Shift+`0`・日本語入力キー・2 つの表の一致・JIS 固有キーである。
//
// **落ちない 1 本を書いておく。** 「ASCII の記号 32 個が全部打てる」は
// **US でも成り立つので、配列を見分けない。** あれが守るのは
// 「記号を打つ道を失わないこと」であって、**どちらの配列かではない。**
//
// **実機の側も落ちる**——`--shell-test` は `bracket_right`（`0x1B`）を打って
// `[` を作っており、US ではあれが `]` になる。**ただし回帰としては置いていない**
// （QEMU を 1 本余計に起こす費用に対して、捕まえる先がホストと同じである）。
#[cfg(feature = "keyboard-us-layout-test")]
const UNSHIFTED: [char; TABLE_LEN] = [
    '\0', '\0', '1', '2', '3', '4', '5', '6', // 0x00-0x07
    '7', '8', '9', '0', '-', '=', '\0', '\0', // 0x08-0x0F (0x0E=BS, 0x0F=Tab)
    'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', // 0x10-0x17
    'o', 'p', '[', ']', '\0', '\0', 'a', 's', // 0x18-0x1F (0x1C=Enter, 0x1D=LCtrl)
    'd', 'f', 'g', 'h', 'j', 'k', 'l', ';', // 0x20-0x27
    '\'', '`', '\0', '\\', 'z', 'x', 'c', 'v', // 0x28-0x2F (0x2A=LShift)
    'b', 'n', 'm', ',', '.', '/', '\0', '\0', // 0x30-0x37 (0x36=RShift, 0x37=keypad *)
    '\0', ' ', '\0', '\0', '\0', '\0', '\0', '\0', // 0x38-0x3F (0x39=Space, 0x3A=Caps)
];

#[cfg(feature = "keyboard-us-layout-test")]
const SHIFTED: [char; TABLE_LEN] = [
    '\0', '\0', '!', '@', '#', '$', '%', '^', // 0x00-0x07
    '&', '*', '(', ')', '_', '+', '\0', '\0', // 0x08-0x0F
    'Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', // 0x10-0x17
    'O', 'P', '{', '}', '\0', '\0', 'A', 'S', // 0x18-0x1F
    'D', 'F', 'G', 'H', 'J', 'K', 'L', ':', // 0x20-0x27
    '"', '~', '\0', '|', 'Z', 'X', 'C', 'V', // 0x28-0x2F
    'B', 'N', 'M', '<', '>', '?', '\0', '\0', // 0x30-0x37
    '\0', ' ', '\0', '\0', '\0', '\0', '\0', '\0', // 0x38-0x3F
];

/// 複数バイトのスキャンコード列を読んでいる途中の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sequence {
    /// 通常。次のバイトは単独のスキャンコードかプレフィックス。
    Idle,
    /// `0xE0` を受けた。次の 1 バイトが拡張キーの本体。
    Extended,
    /// `0xE1` を受けた。残りこのバイト数を無条件に読み捨てる。
    Pause { remaining: u8 },
}

/// スキャンコードを 1 バイトずつ食べて、キーイベントを吐く状態機械。
///
/// 修飾キーの状態を保持するため、**入力の途中で作り直してはいけない**
/// （Shift を押している最中に作り直すと、離したときの break を取りこぼして
/// 押しっぱなし扱いのままになる）。
#[derive(Debug, Clone, Copy)]
pub struct Decoder {
    shift_left: bool,
    shift_right: bool,
    caps_lock: bool,
    /// Ctrl が押されているか（S12 前の手当て、C）。
    ///
    /// **左右を分けない。** Shift は左右を分けているが、あちらは
    /// **どちらが押されたかを数えるため**ではなく、**片方を離しても
    /// もう片方が押されていれば Shift のまま**にするためである。
    /// **Ctrl は右が `0xE0 0x1D` で来るので、接頭辞を落とすと左と区別が付かない。**
    /// **区別しないと決めたので、1 つで持つ**（[`SCANCODE_CTRL`]）。
    ctrl: bool,
    sequence: Sequence,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            shift_left: false,
            shift_right: false,
            caps_lock: false,
            ctrl: false,
            sequence: Sequence::Idle,
        }
    }

    /// Ctrl が押されているか（左右どちらでも）。
    pub const fn ctrl(&self) -> bool {
        self.ctrl
    }

    /// Shift が押されているか（左右どちらでも）。
    pub const fn shift(&self) -> bool {
        self.shift_left || self.shift_right
    }

    pub const fn caps_lock(&self) -> bool {
        self.caps_lock
    }

    /// スキャンコードを 1 バイト与える。
    ///
    /// 報告すべきことが無ければ `None`。修飾キーの更新、キーを離したこと、
    /// 複数バイト列の途中はすべて `None` になる。
    pub fn feed(&mut self, code: u8) -> Option<KeyEvent> {
        match self.sequence {
            Sequence::Pause { remaining } => {
                // 内側の 0xE1 も含めて、数えた分だけ無条件に飲み込む。
                let left = remaining.saturating_sub(1);
                if left == 0 {
                    self.sequence = Sequence::Idle;
                    // 列を最後まで消費し終えた時点で 1 回だけ報告する。
                    // 無言で捨てないため。
                    Some(KeyEvent::Unsupported(PAUSE_PREFIX))
                } else {
                    self.sequence = Sequence::Pause { remaining: left };
                    None
                }
            }
            Sequence::Extended => {
                self.sequence = Sequence::Idle;
                // 拡張キーのうち**左右の矢印だけを扱う**（S12 前の手当て）。
                // 残り（上下・Home・End・右 Ctrl/Alt など）は未対応のままである。
                //
                // 破壊 (S12 前の手当て, keyboard-drop-arrows): 矢印を未対応へ戻す。
                // **シェルの挿入点が動かなくなる**ので、`--shell-test` の
                // 「左へ動かしてから入れた」判定が落ちる。
                #[cfg(not(feature = "keyboard-drop-arrows-test"))]
                if code == SCANCODE_ARROW_LEFT {
                    return Some(KeyEvent::ArrowLeft);
                }
                #[cfg(not(feature = "keyboard-drop-arrows-test"))]
                if code == SCANCODE_ARROW_RIGHT {
                    return Some(KeyEvent::ArrowRight);
                }
                // **上下も同じ feature の下に置く（zi-a）。** 破壊の意味を
                // 「矢印を未対応へ戻す」の 1 つに保つ——鍵ごとに feature を
                // 分けると、名前が主張する範囲と実際の範囲がずれていく。
                #[cfg(not(feature = "keyboard-drop-arrows-test"))]
                if code == SCANCODE_ARROW_UP {
                    return Some(KeyEvent::ArrowUp);
                }
                #[cfg(not(feature = "keyboard-drop-arrows-test"))]
                if code == SCANCODE_ARROW_DOWN {
                    return Some(KeyEvent::ArrowDown);
                }
                // **Delete（zi-f）。** **`keyboard-drop-arrows-test` の下に
                // 置かない**——**あの破壊の意味は「矢印を未対応へ戻す」の
                // 1 つである**（名前が主張する範囲と実際の範囲をずらさない）。
                if code == SCANCODE_DELETE {
                    return Some(KeyEvent::Delete);
                }
                // **右 Ctrl（`0xE0 0x1D`）も Ctrl として扱う（S12 前の手当て、C）。**
                //
                // **接頭辞の有無で左右を区別しない。** 区別すると、
                // **右 Ctrl で Ctrl+C が効かない**という、押した人にしか
                // 分からない差が出る。**左右どちらでも効くほうを採る。**
                //
                // **修飾なので押下と離脱の両方で更新し、報告はしない**
                // （Shift と同じ形。`Unsupported` にも出さない）。
                if code & !BREAK_BIT == SCANCODE_CTRL {
                    self.ctrl = code & BREAK_BIT == 0;
                    return None;
                }
                // 押下のときだけ報告し、離したときは黙る。押下と離しで 2 回数えると、
                // 「押した回数」として見たときに倍になる。
                if code & BREAK_BIT == 0 {
                    Some(KeyEvent::Unsupported(code))
                } else {
                    None
                }
            }
            Sequence::Idle => self.feed_idle(code),
        }
    }

    fn feed_idle(&mut self, code: u8) -> Option<KeyEvent> {
        if code == EXTENDED_PREFIX {
            self.sequence = Sequence::Extended;
            return None;
        }
        if code == PAUSE_PREFIX {
            self.sequence = Sequence::Pause {
                remaining: PAUSE_TRAILING_BYTES,
            };
            return None;
        }

        let released = code & BREAK_BIT != 0;
        let key = code & !BREAK_BIT;

        // 修飾キーは押下と離脱の両方で状態を更新する。
        match key {
            SCANCODE_LEFT_SHIFT => {
                self.shift_left = !released;
                return None;
            }
            SCANCODE_RIGHT_SHIFT => {
                self.shift_right = !released;
                return None;
            }
            SCANCODE_CTRL => {
                // 左 Ctrl。右は `Sequence::Extended` の側で拾う。
                self.ctrl = !released;
                return None;
            }
            SCANCODE_CAPS_LOCK => {
                // **押下でのみトグルする。** 離脱でもトグルすると 1 回押した
                // だけで 2 回反転し、元に戻ってしまう。押しっぱなしのときに
                // リピートが来る場合も、make が来るたびに反転するのが
                // 実機の挙動と一致する。
                if !released {
                    self.caps_lock = !self.caps_lock;
                }
                return None;
            }
            _ => {}
        }

        // 離したことは報告しない。文字が出るのは押下のときだけ。
        if released {
            return None;
        }

        // **Ctrl+C だけを制御文字へ落とす（S12 前の手当て、C）。**
        //
        // **他の Ctrl の組み合わせは従来どおりである**——Ctrl+A は `a` を出す。
        // **一般化して Ctrl+英字を 1..26 へ落とす形も採れるが、採らない。**
        // **使う者がいない機構は検算が置けない**（S9-b-3-1 の診断）。
        // 使う道ができた時点で広げること。
        //
        // **種を足さず `Char` で出す。** Ctrl は修飾であって、キーではない——
        // **矢印（B）で種を足したのは、あれがキーそのものだったからである。**
        // 修飾の族は Shift と Caps Lock で、どちらも種を持たない。
        if self.ctrl && key == SCANCODE_C {
            return Some(KeyEvent::Char(CTRL_C_BYTE as char));
        }

        match key {
            SCANCODE_ENTER => Some(KeyEvent::Enter),
            SCANCODE_BACKSPACE => Some(KeyEvent::Backspace),
            SCANCODE_TAB => Some(KeyEvent::Char('\t')),
            // 破壊 (zi-a, keyboard-drop-esc-test): Esc を未対応へ戻す。
            // **Ring 3 へ `\x1b` が届かなくなる**ので、`--shell-test` の
            // 「Esc `[` `D` の実打鍵が挿入点を動かした」判定が落ちる。
            #[cfg(not(feature = "keyboard-drop-esc-test"))]
            SCANCODE_ESC => Some(KeyEvent::Escape),
            _ => match self.character_for(key) {
                Some(character) => Some(KeyEvent::Char(character)),
                None => Some(KeyEvent::Unsupported(key)),
            },
        }
    }

    /// 押下されたキーに対応する文字。文字を持たないキーは `None`。
    ///
    /// # Shift と Caps Lock の非対称性
    ///
    /// **英字は Shift と Caps Lock の排他的論理和、記号は Shift だけが効く。**
    /// ここを取り違えると、Caps Lock 中に数字段が記号になるという分かりやすい
    /// バグと、Caps Lock + Shift で大文字のままになるという分かりにくいバグの
    /// 両方が出る。
    ///
    /// # 表より先に JIS 固有キーを見る
    ///
    /// **`0x73` と `0x7D` は `TABLE_LEN` の外に居る**ので、範囲の判定に
    /// 先んじて引く（[`JIS_ONLY_KEYS`]）。**順序を逆にすると `None` で
    /// 打ち切られ、2 つのキーが黙って消える。**
    fn character_for(&self, key: u8) -> Option<char> {
        if let Some((_, plain, shifted)) = JIS_ONLY_KEYS.iter().find(|(code, ..)| *code == key) {
            return Some(if self.shift() { *shifted } else { *plain });
        }

        let index = key as usize;
        if index >= TABLE_LEN {
            return None;
        }

        let base = UNSHIFTED[index];
        if base == '\0' {
            return None;
        }

        let use_shifted = if base.is_ascii_alphabetic() {
            // 英字だけ Caps Lock が効く。両方押されていれば打ち消し合う。
            self.shift() != self.caps_lock
        } else {
            // 記号と数字は Shift のみ。Caps Lock は関係しない。
            self.shift()
        };

        let character = if use_shifted { SHIFTED[index] } else { base };
        if character == '\0' {
            None
        } else {
            Some(character)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 押下と離脱を順に食べさせ、報告されたイベントだけを集める補助。
    fn feed_all(decoder: &mut Decoder, codes: &[u8]) -> std::vec::Vec<KeyEvent> {
        extern crate std;
        codes.iter().filter_map(|&c| decoder.feed(c)).collect()
    }

    fn chars(events: &[KeyEvent]) -> std::string::String {
        extern crate std;
        events
            .iter()
            .filter_map(|e| match e {
                KeyEvent::Char(c) => Some(*c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_plain_letter_is_lowercase() {
        let mut decoder = Decoder::new();
        // 0x1E = 'a' の押下、0x9E = 'a' の離脱。
        let events = feed_all(&mut decoder, &[0x1E, 0x9E]);
        assert_eq!(events, [KeyEvent::Char('a')]);
    }

    /// **離したときには文字を出さない。**
    ///
    /// break を押下と同じに扱うと、1 回打つたびに 2 文字出る。
    #[test]
    fn releasing_a_key_produces_nothing() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(0x9E), None, "'a' の break だけでは何も出ない");
    }

    #[test]
    fn shift_makes_letters_uppercase_and_restores_on_release() {
        let mut decoder = Decoder::new();
        // LShift 押下 -> 'a' -> LShift 離脱 -> 'a'
        let events = feed_all(&mut decoder, &[0x2A, 0x1E, 0x9E, 0xAA, 0x1E, 0x9E]);
        assert_eq!(chars(&events), "Aa", "離したら小文字へ戻る");
    }

    /// 左右の Shift は独立に数える。
    ///
    /// 片方を離しただけで解除してしまうと、両手で Shift を押したときに
    /// 途中から小文字になる。
    #[test]
    fn the_two_shift_keys_are_tracked_independently() {
        let mut decoder = Decoder::new();
        // 左右とも押す -> 左だけ離す -> まだ Shift は有効
        feed_all(&mut decoder, &[0x2A, 0x36, 0xAA]);
        assert!(decoder.shift(), "右 Shift がまだ押されている");
        let events = feed_all(&mut decoder, &[0x1E]);
        assert_eq!(chars(&events), "A");
        // 右も離せば解除。
        feed_all(&mut decoder, &[0xB6]);
        assert!(!decoder.shift());
    }

    #[test]
    fn caps_lock_toggles_only_on_press() {
        let mut decoder = Decoder::new();
        assert!(!decoder.caps_lock());
        // 押下 + 離脱で 1 回だけ反転する。離脱でも反転すると元に戻ってしまう。
        feed_all(&mut decoder, &[0x3A, 0xBA]);
        assert!(decoder.caps_lock(), "1 回押したら有効のまま");
        feed_all(&mut decoder, &[0x3A, 0xBA]);
        assert!(!decoder.caps_lock(), "もう 1 回押したら解除");
    }

    /// **英字は Shift と Caps の XOR、記号は Shift のみ。**
    ///
    /// この非対称性が本モジュールで最も取り違えやすい。3 つの状態を並べて
    /// 固定する。
    #[test]
    fn shift_and_caps_lock_are_asymmetric_between_letters_and_symbols() {
        // Caps Lock だけ: 英字は大文字、数字段はそのまま。
        let mut decoder = Decoder::new();
        feed_all(&mut decoder, &[0x3A, 0xBA]); // Caps Lock ON
        let events = feed_all(&mut decoder, &[0x1E, 0x9E, 0x02, 0x82]); // 'a', '1'
        assert_eq!(chars(&events), "A1", "Caps は英字にだけ効く");

        // Caps Lock + Shift: 英字は小文字へ戻り、数字段は記号になる。
        let events = feed_all(&mut decoder, &[0x2A, 0x1E, 0x9E, 0x02, 0x82, 0xAA]);
        assert_eq!(
            chars(&events),
            "a!",
            "英字は XOR で打ち消し、記号は Shift が効く"
        );

        // Shift だけ（Caps 解除）: 英字は大文字、数字段は記号。
        feed_all(&mut decoder, &[0x3A, 0xBA]); // Caps Lock OFF
        let events = feed_all(&mut decoder, &[0x2A, 0x1E, 0x9E, 0x02, 0x82, 0xAA]);
        assert_eq!(chars(&events), "A!");
    }

    #[test]
    fn enter_tab_and_backspace_are_reported_separately() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(0x1C), Some(KeyEvent::Enter));
        assert_eq!(decoder.feed(0x0E), Some(KeyEvent::Backspace));
        assert_eq!(decoder.feed(0x0F), Some(KeyEvent::Char('\t')));
    }

    /// 拡張キー（0xE0 プレフィックス）は押下のときだけ 1 回報告する。
    ///
    /// **上矢印は zi-a で種を得た**ので、未対応の代表は Home（`0xE0 0x47`）へ
    /// 差し替えた。
    #[test]
    fn extended_keys_are_reported_once_on_press() {
        let mut decoder = Decoder::new();
        // 0xE0 0x47 = Home（押下）、0xE0 0xC7 = 同（離脱）。
        assert_eq!(decoder.feed(0xE0), None, "プレフィックス単独では何も出ない");
        assert_eq!(decoder.feed(0x47), Some(KeyEvent::Unsupported(0x47)));
        assert_eq!(decoder.feed(0xE0), None);
        assert_eq!(decoder.feed(0xC7), None, "離脱では報告しない");
    }

    /// **4 方向の矢印がそれぞれの種で届く（zi-a で上下を足した）。**
    #[test]
    fn all_four_arrows_decode_to_their_own_kinds() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(0xE0), None);
        assert_eq!(decoder.feed(0x4B), Some(KeyEvent::ArrowLeft));
        assert_eq!(decoder.feed(0xE0), None);
        assert_eq!(decoder.feed(0x4D), Some(KeyEvent::ArrowRight));
        assert_eq!(decoder.feed(0xE0), None);
        assert_eq!(decoder.feed(0x48), Some(KeyEvent::ArrowUp));
        assert_eq!(decoder.feed(0xE0), None);
        assert_eq!(decoder.feed(0x50), Some(KeyEvent::ArrowDown));
    }

    /// **Delete は拡張キーで、押下のときだけ届く（zi-f）。**
    ///
    /// **素の `0x53` はキーパッドの `.` で、`Unsupported` になる**
    /// ——**同じ番号が接頭辞の有無で別のキーになることを固定する。**
    #[test]
    fn delete_arrives_only_with_the_extended_prefix() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(0xE0), None);
        assert_eq!(decoder.feed(0x53), Some(KeyEvent::Delete));
        assert_eq!(decoder.feed(0xE0), None);
        assert_eq!(decoder.feed(0xD3), None, "離脱では報告しない");

        // **接頭辞なしはキーパッドの `.` である。**
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(0x53), Some(KeyEvent::Unsupported(0x53)));
    }

    /// **Esc は押下で 1 回だけ `Escape` を出す（zi-a）。**
    ///
    /// 離脱（`0x81`）では出さない。文字キーと同じ形である。
    #[test]
    fn esc_is_reported_as_escape_on_press_only() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(0x01), Some(KeyEvent::Escape));
        assert_eq!(decoder.feed(0x81), None, "離脱では報告しない");
    }

    /// **拡張キーの後で通常のキーが壊れない。**
    #[test]
    fn a_normal_key_after_an_extended_sequence_still_decodes() {
        let mut decoder = Decoder::new();
        feed_all(&mut decoder, &[0xE0, 0x48, 0xE0, 0xC8]);
        let events = feed_all(&mut decoder, &[0x1E, 0x9E]);
        assert_eq!(chars(&events), "a");
    }

    /// Pause は 6 バイト列で、**内部に 2 個目の 0xE1 を含む**。
    ///
    /// 0xE0 と同じ「次の 1 バイトを飛ばす」実装だと、内側の 0xE1 で状態機械が
    /// 入れ子になり、以降のバイト境界がずれてキー入力全体が壊れる。数えて
    /// 飲み込む実装であることを固定する。
    #[test]
    fn the_pause_sequence_is_swallowed_whole_including_its_inner_prefix() {
        let mut decoder = Decoder::new();
        let pause = [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5];
        let events = feed_all(&mut decoder, &pause);
        assert_eq!(
            events,
            [KeyEvent::Unsupported(0xE1)],
            "列全体で 1 回だけ報告する"
        );
        // 直後の通常キーが正しく読めること。ここがずれると全部化ける。
        let events = feed_all(&mut decoder, &[0x1E, 0x9E]);
        assert_eq!(chars(&events), "a", "Pause の後もバイト境界が保たれる");
    }

    /// Pause 列の途中に含まれる 0x1D / 0x45 が、Ctrl や NumLock として
    /// 解釈されてしまわないこと。
    #[test]
    fn bytes_inside_the_pause_sequence_are_not_interpreted_as_keys() {
        let mut decoder = Decoder::new();
        feed_all(&mut decoder, &[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]);
        // Shift も Caps も変化していない。
        assert!(!decoder.shift());
        assert!(!decoder.caps_lock());
    }

    #[test]
    fn unsupported_scancodes_are_reported_not_silently_dropped() {
        let mut decoder = Decoder::new();
        // 0x3B = F1。表の範囲内だが文字を持たない。
        assert_eq!(decoder.feed(0x3B), Some(KeyEvent::Unsupported(0x3B)));
        // 0x57 = F11。表の範囲外。
        assert_eq!(decoder.feed(0x57), Some(KeyEvent::Unsupported(0x57)));
        // どちらも離脱では黙る。
        assert_eq!(decoder.feed(0xBB), None);
        assert_eq!(decoder.feed(0xD7), None);
    }

    /// 表の範囲外でも状態機械が壊れないこと。
    #[test]
    fn an_out_of_range_scancode_does_not_break_the_decoder() {
        let mut decoder = Decoder::new();
        decoder.feed(0x7F);
        let events = feed_all(&mut decoder, &[0x1E, 0x9E]);
        assert_eq!(chars(&events), "a");
    }

    /// 実際に打った並びを通しで確認する。`hello!` を打つ。
    #[test]
    fn a_realistic_sequence_decodes_to_the_expected_text() {
        let mut decoder = Decoder::new();
        let codes = [
            0x23, 0xA3, // h
            0x12, 0x92, // e
            0x26, 0xA6, // l
            0x26, 0xA6, // l
            0x18, 0x98, // o
            0x2A, 0x02, 0x82, 0xAA, // Shift + 1 = '!'
        ];
        let events = feed_all(&mut decoder, &codes);
        assert_eq!(chars(&events), "hello!");
    }

    /// 変換表の 2 つの対応が、**記録した 1 箇所を除いて**食い違っていないこと。
    ///
    /// 片方だけに文字があると、Shift の有無で「文字が消える」挙動になる。
    /// **JIS ではそれが 1 箇所だけ正しい**——`0` キーには Shift の刻印が無い。
    ///
    /// **例外を数えて固定する。** 「食い違いがあってもよい」ではなく、
    /// **「食い違うのはここだけ」**を主張する。
    #[test]
    fn the_two_tables_agree_except_at_the_one_key_that_has_no_shift_legend() {
        /// Shift の刻印を持たない唯一のキー（`0`。JIS）。
        const NO_SHIFT_LEGEND: usize = 0x0B;

        for index in 0..TABLE_LEN {
            if index == NO_SHIFT_LEGEND {
                assert_eq!(UNSHIFTED[index], '0', "素の側は 0 である");
                assert_eq!(SHIFTED[index], '\0', "Shift 側には字が無い");
                continue;
            }
            assert_eq!(
                UNSHIFTED[index] == '\0',
                SHIFTED[index] == '\0',
                "scancode {index:#04x} で 2 つの表が食い違っている"
            );
        }
    }

    /// **US と JIS で結果が変わる鍵を名指しで並べる。**
    ///
    /// # なぜ一覧で持つのか
    ///
    /// **表を差し替えた段の主張そのものだからである。** 「JIS になった」は
    /// 表全体を見ても言えず、**US と違う位置を数え上げて初めて言える。**
    /// **US では何だったかを同じ行に置く**——差し替えを戻したくなった人が、
    /// **どこが動くのかをこの一覧だけで読める。**
    ///
    /// **末尾の 2 つは US に対応するキーが無い**（JIS 固有。変換表の外に居る）。
    #[test]
    fn the_keys_that_differ_between_us_and_jis_carry_the_jis_legends() {
        // (スキャンコード, 素, Shift, US では何だったか)
        const DIFFER: &[(u8, char, char, &str)] = &[
            (0x03, '2', '"', "US: 2 / @"),
            (0x07, '6', '&', "US: 6 / ^"),
            (0x08, '7', '\'', "US: 7 / &"),
            (0x09, '8', '(', "US: 8 / *"),
            (0x0A, '9', ')', "US: 9 / ("),
            (0x0C, '-', '=', "US: - / _"),
            (0x0D, '^', '~', "US: = / +"),
            (0x1A, '@', '`', "US: [ / {"),
            (0x1B, '[', '{', "US: ] / }"),
            (0x27, ';', '+', "US: ; / :"),
            (0x28, ':', '*', "US: ' / \""),
            (0x2B, ']', '}', "US: \\ / |"),
            (SCANCODE_JIS_RO, '\\', '_', "US: このキーが無い"),
            (SCANCODE_JIS_YEN, '\\', '|', "US: このキーが無い"),
        ];

        for (code, plain, shifted, was) in DIFFER {
            let mut decoder = Decoder::new();
            assert_eq!(
                decoder.feed(*code),
                Some(KeyEvent::Char(*plain)),
                "{code:#04x} を素で打つ（{was}）"
            );

            let mut decoder = Decoder::new();
            decoder.feed(0x2A); // LShift 押下
            assert_eq!(
                decoder.feed(*code),
                Some(KeyEvent::Char(*shifted)),
                "{code:#04x} を Shift で打つ（{was}）"
            );
        }
    }

    /// **`:` が Shift 無しで打てる。**
    ///
    /// **既定を JIS にした理由そのものである**（`zi` のコマンド行は `:` から
    /// 始まる）。**一覧の中に埋もれさせず、単独で立てる。**
    #[test]
    fn a_colon_needs_no_shift_on_jis() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(0x28), Some(KeyEvent::Char(':')));
    }

    /// **印字可能な ASCII の記号 32 個すべてに経路がある。**
    ///
    /// # 表を読まず、デコーダに打たせて数える
    ///
    /// **表を直接見ると、`character_for` の分岐（JIS 固有キー・範囲の判定・
    /// Shift の規則）を通らない。** **打てるかどうかは、あの分岐まで含めて
    /// 初めて言える**——実際、JIS 固有の 2 キーは表の外に居る。
    #[test]
    fn every_printable_ascii_symbol_can_be_typed() {
        extern crate std;

        /// ASCII の印字可能な記号（空白と英数字を除く 32 個）。
        const SYMBOLS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";

        let mut reachable = std::collections::BTreeSet::new();
        for code in 0u8..0x80 {
            for shift in [false, true] {
                let mut decoder = Decoder::new();
                if shift {
                    decoder.feed(0x2A); // LShift 押下
                }
                if let Some(KeyEvent::Char(character)) = decoder.feed(code) {
                    reachable.insert(character);
                }
            }
        }

        assert_eq!(SYMBOLS.chars().count(), 32, "数え間違いを固定する");
        for symbol in SYMBOLS.chars() {
            assert!(reachable.contains(&symbol), "{symbol:?} を打つ経路が無い");
        }
    }

    /// **`0` に Shift を足しても何も出ない。**
    ///
    /// **JIS の `0` キーには Shift の刻印が無い。** 黙って消すのではなく
    /// [`KeyEvent::Unsupported`] として報告する。
    #[test]
    fn shift_and_zero_report_unsupported_because_the_key_has_no_shift_legend() {
        let mut decoder = Decoder::new();
        decoder.feed(0x2A); // LShift 押下
        assert_eq!(decoder.feed(0x0B), Some(KeyEvent::Unsupported(0x0B)));
    }

    /// **日本語入力のためのキーは、無視すると決めた上で報告する。**
    ///
    /// **半角/全角・かな・変換・無変換の 4 つである。** かな入力も IME も
    /// 無いので文字を持たない。**黙って落とさない**ことをここで固定する。
    #[test]
    fn the_japanese_input_keys_are_reported_as_unsupported() {
        // (スキャンコード, どのキーか)
        const IGNORED: &[(u8, &str)] = &[
            (0x29, "半角/全角"),
            (0x70, "かな"),
            (0x79, "変換"),
            (0x7B, "無変換"),
        ];

        for (code, name) in IGNORED {
            let mut decoder = Decoder::new();
            assert_eq!(
                decoder.feed(*code),
                Some(KeyEvent::Unsupported(*code)),
                "{name}（{code:#04x}）"
            );
        }
    }

    /// **JIS 固有の 2 キーは、変換表の外に居ても状態機械を壊さない。**
    ///
    /// **`TABLE_LEN` を越えたコードは従来 `None` で打ち切られていた。**
    /// 先に引く経路を足したので、**その後で通常のキーが読めることまで見る。**
    #[test]
    fn the_jis_only_keys_live_outside_the_table_and_do_not_break_the_decoder() {
        assert!(SCANCODE_JIS_RO as usize >= TABLE_LEN);
        assert!(SCANCODE_JIS_YEN as usize >= TABLE_LEN);

        let mut decoder = Decoder::new();
        let events = feed_all(
            &mut decoder,
            &[
                SCANCODE_JIS_RO,
                SCANCODE_JIS_RO | BREAK_BIT,
                SCANCODE_JIS_YEN,
                SCANCODE_JIS_YEN | BREAK_BIT,
                0x1E,
                0x9E,
            ],
        );
        assert_eq!(chars(&events), "\\\\a");
    }
}
