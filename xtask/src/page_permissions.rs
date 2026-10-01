//! ページの権限の一覧を読んで、2 つを比べる（純粋な論理。2026-10-01）。
//!
//! 一覧を出すのはカーネルである（`kernel/src/page_survey.rs`。`page-permissions-dump` の feature を付けると、
//! 起動の途中の時点ごとに全部の行をシリアルへ出す）。ここは、その行を取り出し、前と後の一覧を比べて、
//! 違いを「時点・領域の名前・ページの大きさ・変わった権限（前 -> 後）」の形で並べる。
//! 権限の設定を 1 か所へ寄せる前と後で、どのページの権限も変わっていないことを確かめるのに使う
//! （`ADR-0071` の手順 3）。
//!
//! # 行の形
//!
//! ```text
//! <時点> | <領域> | <大きさ> | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=0 x=1) | pages=12
//! <時点> | <領域> | absent | nothing is mapped here, as it should be | pages=0
//! ```
//!
//! - **`pages=~12` のように `~` が付いた行は、ページ数を比べない**（カーネルの像の区画や CPU ごとのスタックのように、
//!   コードの量や CPU の数でページ数が動く領域）。権限は比べる。
//! - 行の末尾の ` first=0x...`（名前の無い範囲の先頭の番地）は読み捨てる。番地はビルドごとに動く。
//!
//! # 同じ名前の時点が 2 度出るとき
//!
//! **同じプログラムを 2 度走らせると、同じ名前の時点が 2 度出る。** 出た順に 2 つ目から `(#2)` を付けて分ける。
//! **時点の切れ目は、一覧の中の空の行である**——カーネルは時点ごとに行を出した後で要約の 1 行を出すので、
//! シリアルから取り出すときに、要約の行を空の行に置き換える。同じ名前の時点が続けて 2 度出ても分けられる。

/// カーネルが一覧の行の頭に付ける目印。
pub const LINE_MARK: &str = "page-perms: ";

/// 行の欄の区切り。
const SEPARATOR: &str = " | ";

/// 一覧の 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// 時点（同じ名前の 2 つ目からは `(#2)` が付く）。
    pub moment: String,
    /// 領域の名前。
    pub region: String,
    /// ページの大きさ（`4K`・`2M`・`1G`）。何も写っていない領域の行は `absent`。
    pub size: String,
    /// 権限の欄（名前と値）。途中の階層の分は `tables.w` のように名前の頭に `tables.` が付く。
    pub fields: Vec<(String, String)>,
    /// ページ数（4KiB で数える）。
    pub pages: u64,
    /// ページ数を比べるか。
    pub pages_compared: bool,
}

impl Row {
    fn key(&self) -> (&str, &str, &str) {
        (&self.moment, &self.region, &self.size)
    }

    fn place(&self) -> String {
        format!("{} | {} | {}", self.moment, self.region, self.size)
    }

    fn permissions(&self) -> String {
        self.fields
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// シリアルのログから、一覧の行（目印の後ろ）を取り出す。**要約の行（区切りを持たない行）は、時点の切れ目として
/// 空の行に置き換える。**
pub fn rows_in_serial(serial: &str) -> Vec<String> {
    serial
        .lines()
        .filter_map(|line| line.split_once(LINE_MARK).map(|(_, rest)| rest.trim_end()))
        .map(|rest| {
            if rest.contains(SEPARATOR) {
                rest.to_string()
            } else {
                String::new()
            }
        })
        .collect()
}

/// 一覧を読む。**読めない行が在れば、その行を挙げて誤りにする**（黙って飛ばすと、比べたつもりで比べていない）。
pub fn parse(listing: &str) -> Result<Vec<Row>, String> {
    let mut rows: Vec<Row> = Vec::new();
    // 時点の名前ごとに、何度目の塊か。
    let mut seen: Vec<(String, usize)> = Vec::new();
    let mut current: Option<String> = None;
    for line in listing.lines() {
        if line.trim().is_empty() {
            // 時点の切れ目。次の行は、同じ名前でも新しい塊である。
            current = None;
            continue;
        }
        let parts: Vec<&str> = line.split(SEPARATOR).collect();
        let [moment, region, size, permissions, pages] = parts[..] else {
            return Err(format!("not a row of 5 columns: {line:?}"));
        };
        if current.as_deref() != Some(moment) {
            current = Some(moment.to_string());
            match seen.iter_mut().find(|(name, _)| name == moment) {
                Some((_, count)) => *count += 1,
                None => seen.push((moment.to_string(), 1)),
            }
        }
        let occurrence = seen
            .iter()
            .find(|(name, _)| name == moment)
            .map_or(1, |(_, count)| *count);
        let moment = if occurrence > 1 {
            format!("{moment} (#{occurrence})")
        } else {
            moment.to_string()
        };
        let pages = pages
            .strip_prefix("pages=")
            .ok_or_else(|| format!("the last column is not pages=N: {line:?}"))?;
        // 名前の無い範囲の先頭の番地は読み捨てる。
        let pages = pages
            .split_once(" first=")
            .map_or(pages, |(count, _)| count);
        let (pages, pages_compared) = match pages.strip_prefix('~') {
            Some(count) => (count, false),
            None => (pages, true),
        };
        let pages: u64 = pages
            .parse()
            .map_err(|_| format!("the page count is not a number: {line:?}"))?;
        rows.push(Row {
            moment,
            region: region.to_string(),
            size: size.to_string(),
            fields: fields_of(permissions)
                .ok_or_else(|| format!("the permissions are not name=value pairs: {line:?}"))?,
            pages,
            pages_compared,
        });
    }
    Ok(rows)
}

/// 権限の欄を、名前と値の並びにする。何も写っていない領域の行は、欄を持たない。
fn fields_of(text: &str) -> Option<Vec<(String, String)>> {
    if !text.contains('=') {
        return Some(Vec::new());
    }
    let (plain, tables) = match text.split_once(" tables(") {
        Some((plain, tables)) => (plain, tables.strip_suffix(')')?),
        None => (text, ""),
    };
    let mut fields = Vec::new();
    for token in plain.split_whitespace() {
        let (name, value) = token.split_once('=')?;
        fields.push((name.to_string(), value.to_string()));
    }
    for token in tables.split_whitespace() {
        let (name, value) = token.split_once('=')?;
        fields.push((format!("tables.{name}"), value.to_string()));
    }
    Some(fields)
}

/// 2 つの一覧の違いを並べる。**空なら、どの行の権限も（比べるページ数も）同じである。**
///
/// 1 つの違いは 1 行で、「時点 | 領域 | 大きさ: 欄 前 -> 後」の形である。片方にだけ在る行は `gone`（前にだけ在る）か
/// `new`（後にだけ在る）で出す。
///
/// # 行の突き合わせ方
///
/// **同じ「時点・領域・大きさ」の行が、権限の違いで 2 つ以上在ることがある**（恒等の中の、キャッシュ無効で写した
/// 範囲など）。そこで、同じ「時点・領域・大きさ」の組の中で、
///
/// 1. 権限が全部同じ行どうしを先に対にする（対になった行は、ページ数だけを比べる）
/// 2. 残りが前と後に 1 行ずつなら、その 2 行を対にして、変わった欄を出す
/// 3. 残りがそれ以外の数なら、どれがどれに変わったかは決められないので、`gone` と `new` で出す
pub fn differences(before: &[Row], after: &[Row]) -> Vec<String> {
    let mut found = Vec::new();
    // 組（時点・領域・大きさ）を、出た順に 1 度ずつ見る。
    let mut keys: Vec<(&str, &str, &str)> = Vec::new();
    for row in before.iter().chain(after) {
        if !keys.contains(&row.key()) {
            keys.push(row.key());
        }
    }
    for key in keys {
        let mut old: Vec<&Row> = before.iter().filter(|row| row.key() == key).collect();
        let mut new: Vec<&Row> = after.iter().filter(|row| row.key() == key).collect();
        // 1. 権限が全部同じ行どうし。
        let mut index = 0;
        while index < old.len() {
            match new.iter().position(|row| row.fields == old[index].fields) {
                Some(at) => {
                    compare_pages(old[index], new[at], &mut found);
                    old.remove(index);
                    new.remove(at);
                }
                None => index += 1,
            }
        }
        // 2. 残りが 1 行ずつ。
        if let ([was], [now]) = (&old[..], &new[..]) {
            for (name, value) in &was.fields {
                match now.fields.iter().find(|(other, _)| other == name) {
                    Some((_, changed)) if changed == value => {}
                    Some((_, changed)) => {
                        found.push(format!("{}: {name} {value} -> {changed}", was.place()))
                    }
                    None => found.push(format!(
                        "{}: {name} {value} -> (no such column)",
                        was.place()
                    )),
                }
            }
            for (name, value) in &now.fields {
                if !was.fields.iter().any(|(other, _)| other == name) {
                    found.push(format!(
                        "{}: {name} (no such column) -> {value}",
                        was.place()
                    ));
                }
            }
            compare_pages(was, now, &mut found);
            continue;
        }
        // 3. それ以外。
        for row in old {
            found.push(format!(
                "{}: gone (was {} pages={})",
                row.place(),
                row.permissions(),
                row.pages
            ));
        }
        for row in new {
            found.push(format!(
                "{}: new ({} pages={})",
                row.place(),
                row.permissions(),
                row.pages
            ));
        }
    }
    found
}

/// 対になった 2 行のページ数を比べる。**どちらも「比べる」行のときだけ、数の違いを出す。**
fn compare_pages(old: &Row, new: &Row, found: &mut Vec<String>) {
    if old.pages_compared != new.pages_compared {
        found.push(format!(
            "{}: the page count is compared = {} -> {}",
            old.place(),
            old.pages_compared,
            new.pages_compared
        ));
    } else if old.pages_compared && old.pages != new.pages {
        found.push(format!(
            "{}: pages {} -> {}",
            old.place(),
            old.pages,
            new.pages
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "\
the boot table | identity | 2M | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=0 x=1) | pages=~262144
before interrupts are enabled | kernel text | 4K | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=0 x=1) | pages=~457
before interrupts are enabled | guard pages below the stacks | absent | nothing is mapped here, as it should be | pages=0
user program hello | segment 0 | 4K | w=0 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1
user program hello | stack | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1
";

    /// シリアルのログから取り出すのは、目印の後ろに区切りを持つ行だけである。**要約の行と、ほかの行は取らない。**
    #[test]
    fn only_the_rows_are_taken_out_of_the_serial_log() {
        let serial = "\
[INFO] page-perms: the boot table | identity | 2M | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=0 x=1) | pages=~262144\r
[INFO] page-perms: the boot table: 3 row(s), 0 unnamed, 0 mapped where nothing should be, 0 dropped, digest=0123456789abcdef
[INFO] paging: CR3 switch instruction executed
[INFO] page-perms: user program hello | stack | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1
";
        let rows = rows_in_serial(serial);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("the boot table | identity | 2M | "));
        assert!(rows[0].ends_with("pages=~262144"));
        // 要約の行は、時点の切れ目として空の行になる。
        assert_eq!(rows[1], "");
        assert!(rows[2].starts_with("user program hello | stack"));
    }

    /// 行を読む——権限の欄、途中の階層の欄（`tables.` が付く）、ページ数、比べないページ数（`~`）、何も写っていない領域。
    #[test]
    fn a_listing_is_read_into_rows() {
        let rows = parse(LISTING).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].moment, "the boot table");
        assert_eq!((rows[0].pages, rows[0].pages_compared), (262144, false));
        assert_eq!(rows[0].fields[0], ("w".to_string(), "1".to_string()));
        assert!(rows[0]
            .fields
            .contains(&("tables.u".to_string(), "0".to_string())));
        assert_eq!(rows[0].fields.len(), 9);
        assert_eq!(rows[2].size, "absent");
        assert!(rows[2].fields.is_empty());
        assert_eq!((rows[3].pages, rows[3].pages_compared), (1, true));

        // 名前の無い範囲の先頭の番地は読み捨てる。
        let unnamed = parse(
            "m | (unnamed) | 4K | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=0 x=1) | pages=3 first=0xffff800000001000\n",
        )
        .unwrap();
        assert_eq!(unnamed[0].pages, 3);

        // 読めない行は、行を挙げて誤りにする。
        assert!(parse("only | three | columns\n")
            .unwrap_err()
            .contains("5 columns"));
        assert!(parse("m | r | 4K | w=1 | pages=many\n")
            .unwrap_err()
            .contains("not a number"));
        assert!(parse("m | r | 4K | w=1 | 12\n")
            .unwrap_err()
            .contains("pages=N"));
    }

    /// **同じ一覧どうしは違いが無い。権限を 1 つ変えると、時点・領域・大きさ・欄の名前つきで 1 行出る。**
    #[test]
    fn one_changed_permission_is_named_with_its_moment_region_and_column() {
        let before = parse(LISTING).unwrap();
        assert!(differences(&before, &before).is_empty());

        let changed = LISTING.replace(
            "user program hello | segment 0 | 4K | w=0",
            "user program hello | segment 0 | 4K | w=1",
        );
        assert_eq!(
            differences(&before, &parse(&changed).unwrap()),
            vec!["user program hello | segment 0 | 4K: w 0 -> 1".to_string()]
        );

        // 途中の階層の権限が変わったときは、欄の名前に `tables.` が付く。
        let changed = LISTING.replace(
            "kernel text | 4K | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=0 x=1)",
            "kernel text | 4K | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1)",
        );
        assert_eq!(
            differences(&before, &parse(&changed).unwrap()),
            vec!["before interrupts are enabled | kernel text | 4K: tables.u 0 -> 1".to_string()]
        );
    }

    /// **ページ数は、比べる行でだけ違いになる。** 片方にだけ在る行は `gone` と `new` で出る——見張りのページに
    /// 何か写った形は、「何も写っていない」の行が消え、写っている行が増える。
    #[test]
    fn page_counts_and_rows_on_one_side_only_are_reported() {
        let before = parse(LISTING).unwrap();
        // 比べないページ数（`~`）が変わっても、違いにならない。
        let grown = LISTING.replace("pages=~457", "pages=~460");
        assert!(differences(&before, &parse(&grown).unwrap()).is_empty());
        // 比べるページ数が変わると、違いになる。
        let grown = LISTING.replace(
            "user program hello | stack | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1",
            "user program hello | stack | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=2",
        );
        assert_eq!(
            differences(&before, &parse(&grown).unwrap()),
            vec!["user program hello | stack | 4K: pages 1 -> 2".to_string()]
        );

        let mapped = LISTING.replace(
            "guard pages below the stacks | absent | nothing is mapped here, as it should be | pages=0",
            "guard pages below the stacks (MAPPED, but nothing should be here) | 4K | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=0 x=1) | pages=1",
        );
        let found = differences(&before, &parse(&mapped).unwrap());
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].contains("guard pages below the stacks | absent: gone"));
        assert!(found[1].contains("(MAPPED, but nothing should be here) | 4K: new (w=1"));
    }

    /// **同じ「時点・領域・大きさ」に、権限の違う行が 2 つ在っても、取り違えない。** 権限が同じ行を先に対にするので、
    /// 変わっていない行を「変わった」と言わない。片方だけが変われば、その 1 行だけが出る。
    #[test]
    fn rows_that_share_a_place_but_differ_in_permissions_are_paired_by_their_permissions() {
        let listing = "\
m | identity | 2M | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=0 x=1) | pages=~64512
m | identity | 2M | w=1 u=0 x=1 cache=2 g=0 shared=0 tables(w=1 u=0 x=1) | pages=~1024
m | trial | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=2
m | trial | 4K | w=0 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1
";
        let before = parse(listing).unwrap();
        assert!(differences(&before, &before).is_empty());
        // 行の順が入れ替わっても、違いは無い。
        let mut swapped = before.clone();
        swapped.swap(0, 1);
        swapped.swap(2, 3);
        assert!(differences(&before, &swapped).is_empty());

        // 読み取り専用の 1 ページが書き込み可になると、書き込み可の行が 3 ページになり、読み取り専用の行が消える。
        let merged = "\
m | identity | 2M | w=1 u=0 x=1 cache=0 g=0 shared=0 tables(w=1 u=0 x=1) | pages=~64512
m | identity | 2M | w=1 u=0 x=1 cache=2 g=0 shared=0 tables(w=1 u=0 x=1) | pages=~1024
m | trial | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=3
";
        let found = differences(&before, &parse(merged).unwrap());
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0], "m | trial | 4K: pages 2 -> 3");
        assert!(
            found[1].starts_with("m | trial | 4K: gone (was w=0 u=1"),
            "{found:?}"
        );

        // キャッシュ無効の行だけが変わると、その行の欄が出る（もう片方の行は対になって消える）。
        let changed = listing.replace(
            "cache=2 g=0 shared=0 tables(w=1 u=0 x=1) | pages=~1024",
            "cache=6 g=0 shared=0 tables(w=1 u=0 x=1) | pages=~1024",
        );
        assert_eq!(
            differences(&before, &parse(&changed).unwrap()),
            vec!["m | identity | 2M: cache 2 -> 6".to_string()]
        );
    }

    /// 同じ名前の時点が離れて 2 度出たら、2 つ目に `(#2)` を付けて、別の時点として比べる。
    #[test]
    fn a_moment_that_comes_twice_is_told_apart() {
        let listing = "\
user program /bin/cat | stack | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1
user program /bin/ls | stack | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1
user program /bin/cat | stack | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1
user program /bin/cat | heap | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=2
";
        let rows = parse(listing).unwrap();
        let moments: Vec<&str> = rows.iter().map(|row| row.moment.as_str()).collect();
        assert_eq!(
            moments,
            vec![
                "user program /bin/cat",
                "user program /bin/ls",
                "user program /bin/cat (#2)",
                "user program /bin/cat (#2)",
            ]
        );
        // 2 度目の側だけを変えると、2 度目の名前で出る。
        let changed = listing.replace("heap | 4K | w=1", "heap | 4K | w=0");
        assert_eq!(
            differences(&rows, &parse(&changed).unwrap()),
            vec!["user program /bin/cat (#2) | heap | 4K: w 1 -> 0".to_string()]
        );

        // 同じ名前の時点が続けて 2 度出ても、間の空の行（要約の行の跡）で分ける。
        let twice = "\
user program /bin/cat | stack | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1

user program /bin/cat | stack | 4K | w=1 u=1 x=1 cache=0 g=0 shared=0 tables(w=1 u=1 x=1) | pages=1
";
        let rows = parse(twice).unwrap();
        assert_eq!(rows[0].moment, "user program /bin/cat");
        assert_eq!(rows[1].moment, "user program /bin/cat (#2)");
    }
}
