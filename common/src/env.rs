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
