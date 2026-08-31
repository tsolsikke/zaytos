//! 環境の 1 行の純粋な判定（f-1。`ADR-0052` / `ADR-0053`）。
//!
//! # なぜ `common` に在るのか
//!
//! **カーネルとシェルが同じ規則で名前を見るためである。**
//! **カーネルは `/etc/environment` の行を読み、シェルは `export NAME=VALUE` を受ける**
//! ——**別の規則にすると、ファイルへは置けるのに `export` では断られる名前ができる。**
//!
//! **取り込み方は `ADR-0045` の形である**（`#[path]` で取り込む。`crate::` を参照しない）。
//!
//! # ホストで固定できる
//!
//! **ここに在るのは純粋な論理だけで、`core` しか使わない**
//! （`CLAUDE.md` の絶対ルール 5）。

/// 1 プロセスに渡せる環境の要素数の上限（EV。ADR-0041）。
///
/// **`argv` の上限（`MAX_ARGV`）と同じ 8 にしてある。** **いま積むのは 1 つだけである**
/// （`TERM`）。**8 はその 8 倍で、表と文字列がスタックの 1 ページに収まる
/// 範囲である。** 越えたらカーネルが `ArgumentsTooLong` で拒む
/// ——**黙って切り詰めない。**
pub const MAX_ENVP: usize = 8;

/// 1 行の上限（f-1。`ADR-0052`）。
///
/// **`PATH` が伸びても収まる大きさである。** **越えた行は落とす**
/// （`ADR-0052` の Decision 3）。
pub const ENV_LINE_MAX: usize = 128;

/// 1 行を読んだ結果（f-1）。**純粋な判定なので、ホストで固定できる。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnvLine {
    /// 空行と `#` で始まる行。**壊れではない。** 黙って飛ばす。
    Ignore,
    /// 採る。
    Take,
    /// 壊れている。**その行だけ落とし、理由を出す**（`ADR-0052`）。
    Reject(EnvReject),
}

/// 落とす理由（f-1）。**出す文言のためだけに分けてある**
/// ——**黙って落とさないのが `ADR-0052` の Decision 3 である。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnvReject {
    /// `=` が無い。
    NoEquals,
    /// `=` の左が空。
    EmptyName,
    /// 名前に使えない字が在る。
    BadName,
    /// 行が [`ENV_LINE_MAX`] を超える。
    TooLong,
}

/// 1 行を判定する（f-1）。
///
/// # 名前の規則は `ADR-0049` と同じものを使う
///
/// **`[A-Za-z_][A-Za-z0-9_]*` である。** **シェルが `$NAME` で引ける名前と、
/// ここで受ける名前を別にしない**——**別にすると、置けるのに引けない名前が
/// できる。**
///
/// **値は何でもよい。** **空でもよい**（`NAME=` は「空の値」である）。
pub fn classify_env_line(line: &[u8]) -> EnvLine {
    if line.is_empty() || line[0] == b'#' {
        return EnvLine::Ignore;
    }
    if line.len() > ENV_LINE_MAX {
        return EnvLine::Reject(EnvReject::TooLong);
    }
    let Some(equals) = line.iter().position(|byte| *byte == b'=') else {
        return EnvLine::Reject(EnvReject::NoEquals);
    };
    let name = &line[..equals];
    if name.is_empty() {
        return EnvLine::Reject(EnvReject::EmptyName);
    }
    if !(name[0].is_ascii_alphabetic() || name[0] == b'_') {
        return EnvLine::Reject(EnvReject::BadName);
    }
    if !name[1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return EnvLine::Reject(EnvReject::BadName);
    }
    EnvLine::Take
}

/// 行の末尾の `\r` と、前後の空白を落とす（f-1）。
///
/// **`\r` を落とすのは、運用者が別の機械で編集する道が在るためである**
/// （`disk0.img` は持ち越すので、外の道具で触れる）。
pub fn trim_env_line(line: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = line.len();
    while start < end && (line[start] == b' ' || line[start] == b'\t') {
        start += 1;
    }
    while end > start && (line[end - 1] == b' ' || line[end - 1] == b'\t' || line[end - 1] == b'\r')
    {
        end -= 1;
    }
    &line[start..end]
}

#[cfg(test)]
mod tests {
    use super::{classify_env_line, trim_env_line, EnvLine, EnvReject, ENV_LINE_MAX};

    /// 環境の 1 行の判定（f-1。`ADR-0052` の Decision 3）。
    ///
    /// **主張は「落とす側」が主である。** **採る側だけを見ると、
    /// 何でも採る形が通る。**
    #[test]
    fn an_environment_line_is_taken_ignored_or_rejected() {
        assert_eq!(classify_env_line(b"TERM=zaytos"), EnvLine::Take);
        assert_eq!(classify_env_line(b"_X=1"), EnvLine::Take);
        // **値は空でもよい。** `NAME=` は「空の値」である。
        assert_eq!(classify_env_line(b"EMPTY="), EnvLine::Take);
        // **値に `=` が在ってもよい**（最初の `=` で割る）。
        assert_eq!(classify_env_line(b"A=b=c"), EnvLine::Take);

        // 飛ばす。**壊れではない。**
        assert_eq!(classify_env_line(b""), EnvLine::Ignore);
        assert_eq!(classify_env_line(b"# comment"), EnvLine::Ignore);

        // 落とす。
        assert_eq!(
            classify_env_line(b"NOEQUALS"),
            EnvLine::Reject(EnvReject::NoEquals)
        );
        assert_eq!(
            classify_env_line(b"=value"),
            EnvLine::Reject(EnvReject::EmptyName)
        );
        assert_eq!(
            classify_env_line(b"1BAD=x"),
            EnvLine::Reject(EnvReject::BadName)
        );
        assert_eq!(
            classify_env_line(b"A-B=x"),
            EnvLine::Reject(EnvReject::BadName)
        );
        let mut long = [b'A'; ENV_LINE_MAX + 1];
        long[1] = b'=';
        assert_eq!(
            classify_env_line(&long),
            EnvLine::Reject(EnvReject::TooLong)
        );
        // **上限ちょうどは採る。** **境界の両側を見る。**
        assert_eq!(classify_env_line(&long[..ENV_LINE_MAX]), EnvLine::Take);
    }

    /// 行の前後を落とす（f-1）。
    ///
    /// **`\r` を落とすのは、像を外の道具で編集する道が在るためである。**
    #[test]
    fn an_environment_line_is_trimmed_on_both_sides() {
        assert_eq!(trim_env_line(b"  TERM=zaytos  "), b"TERM=zaytos");
        assert_eq!(trim_env_line(b"TERM=zaytos\r"), b"TERM=zaytos");
        assert_eq!(trim_env_line(b"\tA=1 \r"), b"A=1");
        // **値の中の空白は落とさない。** 端だけである。
        assert_eq!(trim_env_line(b"A=b c"), b"A=b c");
        assert_eq!(trim_env_line(b"   "), b"");
    }
}

/// `export` が断る理由（f-2。`ADR-0053` の Decision 5）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnvSetError {
    /// 行そのものが壊れている（名前の規則・長さ）。
    Line(EnvReject),
    /// 表が満杯である。**黙って切り詰めない**（`ADR-0041` の Decision 5 と同じ形）。
    Full,
}

/// 環境の表（f-2。`ADR-0053`）。
///
/// # 誰が使うのか
///
/// **カーネルとシェルの両方である。** カーネルは `/etc/environment` の行を
/// [`EnvTable::push`] で積み、シェルは `export` を [`EnvTable::set`] で受ける。
///
/// # 2 つの入口が在る理由
///
/// **`push` は追記だけで、同じ名前が 2 度来ても 2 行になる**——**源のファイルの
/// 振る舞いを変えない**（`ADR-0052` は「先頭から採り、残りを落とす」と決めた）。
///
/// **`set` は上書きする**——**`export` は同じ名前を何度も打てるほうが自然で、
/// 打つたびに枠が減る形は驚きが大きい。**
///
/// # NUL を持って置く
///
/// **行の直後に必ず NUL を置く**（[`EnvTable::line_with_nul`]）。
/// **`spawn` へ渡すのは NUL 終端の文字列の配列だからである**——
/// **渡す直前に写しを作る形にすると、写し先をもう 1 つ持つことになる。**
pub struct EnvTable {
    lines: [[u8; ENV_LINE_MAX + 1]; MAX_ENVP],
    lens: [usize; MAX_ENVP],
    count: usize,
}

impl Default for EnvTable {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvTable {
    /// 空の表。
    pub const fn new() -> Self {
        Self {
            lines: [[0; ENV_LINE_MAX + 1]; MAX_ENVP],
            lens: [0; MAX_ENVP],
            count: 0,
        }
    }

    /// 入っている本数。
    pub fn count(&self) -> usize {
        self.count
    }

    /// 全部落とす。
    pub fn clear(&mut self) {
        self.count = 0;
    }

    /// `index` 番目の行（NUL を含まない）。**範囲外なら空である。**
    pub fn line(&self, index: usize) -> &[u8] {
        if index >= self.count {
            return b"";
        }
        &self.lines[index][..self.lens[index]]
    }

    /// `index` 番目の行（末尾の NUL を含む）。**範囲外なら空である。**
    ///
    /// **`spawn` へ渡すポインタはここから採る。**
    pub fn line_with_nul(&self, index: usize) -> &[u8] {
        if index >= self.count {
            return b"";
        }
        &self.lines[index][..self.lens[index] + 1]
    }

    /// 追記する。**入らなければ偽を返す。** **同じ名前でも上書きしない。**
    pub fn push(&mut self, line: &[u8]) -> bool {
        if self.count >= MAX_ENVP || line.len() > ENV_LINE_MAX {
            return false;
        }
        self.write(self.count, line);
        self.count += 1;
        true
    }

    /// `NAME=VALUE` を置く（`export`）。**同じ名前が在れば上書きし、枠を消費しない。**
    ///
    /// **断るのは 2 つの場合だけである**——**行が壊れているか、満杯か。**
    /// **どちらでも表は変わらない**（部分的に適用しない）。
    pub fn set(&mut self, line: &[u8]) -> Result<(), EnvSetError> {
        match classify_env_line(line) {
            // **`export` に「無視する行」は無い。** 空行も `#` で始まる行も
            // `=` を持たないので、`NoEquals` として断る。
            EnvLine::Ignore => return Err(EnvSetError::Line(EnvReject::NoEquals)),
            EnvLine::Reject(reason) => return Err(EnvSetError::Line(reason)),
            EnvLine::Take => {}
        }
        let equals = match line.iter().position(|byte| *byte == b'=') {
            Some(at) => at,
            // `classify_env_line` が `Take` を返した時点で `=` は在る。
            None => return Err(EnvSetError::Line(EnvReject::NoEquals)),
        };
        if let Some(index) = self.index_of(&line[..equals]) {
            self.write(index, line);
            return Ok(());
        }
        if self.count >= MAX_ENVP {
            return Err(EnvSetError::Full);
        }
        self.write(self.count, line);
        self.count += 1;
        Ok(())
    }

    /// 名前で値を引く。**無ければ `None`。**
    ///
    /// **前方一致では引かない**——`TERM` は `TERMINFO` に当たらない
    /// （`userlib::environment` と同じ規則である）。
    pub fn value(&self, name: &[u8]) -> Option<&[u8]> {
        let index = self.index_of(name)?;
        Some(&self.lines[index][name.len() + 1..self.lens[index]])
    }

    /// 名前の行の位置。
    fn index_of(&self, name: &[u8]) -> Option<usize> {
        (0..self.count).find(|index| {
            let line = &self.lines[*index][..self.lens[*index]];
            line.len() > name.len() && line[name.len()] == b'=' && &line[..name.len()] == name
        })
    }

    /// 1 行を書き、直後に NUL を置く。
    fn write(&mut self, index: usize, line: &[u8]) {
        self.lines[index][..line.len()].copy_from_slice(line);
        self.lines[index][line.len()] = 0;
        self.lens[index] = line.len();
    }
}

#[cfg(test)]
mod table_tests {
    use super::{EnvReject, EnvSetError, EnvTable, ENV_LINE_MAX, MAX_ENVP};

    /// `export` は上書きで、枠を消費しない（f-2。`ADR-0053` の Decision 5）。
    #[test]
    fn setting_the_same_name_twice_overwrites_and_keeps_the_slot() {
        let mut table = EnvTable::new();
        table.set(b"TERM=zaytos").unwrap();
        assert_eq!(table.count(), 1);
        table.set(b"TERM=dumb").unwrap();
        assert_eq!(table.count(), 1);
        assert_eq!(table.value(b"TERM"), Some(&b"dumb"[..]));
    }

    /// 満杯なら断る。**表は変わらない**（黙って落とさない）。
    #[test]
    fn setting_a_new_name_when_full_is_refused_and_changes_nothing() {
        let mut table = EnvTable::new();
        for index in 0..MAX_ENVP {
            let line = [b'A' + index as u8, b'=', b'1'];
            assert!(table.set(&line).is_ok());
        }
        assert_eq!(table.count(), MAX_ENVP);
        assert_eq!(table.set(b"Z=1"), Err(EnvSetError::Full));
        assert_eq!(table.count(), MAX_ENVP);
        assert_eq!(table.value(b"Z"), None);
        // **満杯でも上書きは通る。** 枠を要らないからである。
        assert!(table.set(b"A=2").is_ok());
        assert_eq!(table.value(b"A"), Some(&b"2"[..]));
    }

    /// 壊れた行は断る。**表は変わらない。**
    #[test]
    fn setting_a_broken_line_is_refused_and_changes_nothing() {
        let mut table = EnvTable::new();
        assert_eq!(
            table.set(b"1BAD=x"),
            Err(EnvSetError::Line(EnvReject::BadName))
        );
        assert_eq!(
            table.set(b"NOEQUALS"),
            Err(EnvSetError::Line(EnvReject::NoEquals))
        );
        // **空行と `#` は「無視」ではなく「断る」である**（`export` の入口には
        // 読み飛ばす行が無い）。
        assert_eq!(table.set(b""), Err(EnvSetError::Line(EnvReject::NoEquals)));
        let mut long = [b'A'; ENV_LINE_MAX + 1];
        long[1] = b'=';
        assert_eq!(table.set(&long), Err(EnvSetError::Line(EnvReject::TooLong)));
        assert_eq!(table.count(), 0);
    }

    /// 名前で引く。**前方一致では引かない。**
    #[test]
    fn a_value_is_looked_up_by_the_whole_name() {
        let mut table = EnvTable::new();
        table.set(b"TERMINFO=/usr").unwrap();
        assert_eq!(table.value(b"TERM"), None);
        table.set(b"TERM=zaytos").unwrap();
        assert_eq!(table.value(b"TERM"), Some(&b"zaytos"[..]));
        // **空の値も引ける**（`NAME=` は「空の値」である）。
        table.set(b"EMPTY=").unwrap();
        assert_eq!(table.value(b"EMPTY"), Some(&b""[..]));
    }

    /// 行は NUL で終わる。**`spawn` へ渡すポインタがそれを要る。**
    #[test]
    fn every_line_carries_a_trailing_nul() {
        let mut table = EnvTable::new();
        table.set(b"PATH=/bin").unwrap();
        assert_eq!(table.line(0), b"PATH=/bin");
        assert_eq!(table.line_with_nul(0), b"PATH=/bin\0");
        // **短い行で上書きしても、NUL の位置は追う。**
        table.set(b"PATH=/").unwrap();
        assert_eq!(table.line_with_nul(0), b"PATH=/\0");
    }

    /// `push` は上書きしない。**源のファイルの振る舞いを変えない。**
    #[test]
    fn pushing_the_same_name_twice_keeps_both_lines() {
        let mut table = EnvTable::new();
        assert!(table.push(b"TERM=a"));
        assert!(table.push(b"TERM=b"));
        assert_eq!(table.count(), 2);
        // **引くのは先に置いたほうである**（`ADR-0052` の「先頭から採る」）。
        assert_eq!(table.value(b"TERM"), Some(&b"a"[..]));
    }
}
