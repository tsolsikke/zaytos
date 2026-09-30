//! 環境変数の旧い名前（OS の名前を変える段階の R5。`ADR-0073`）。
//!
//! **環境変数の名前は `ZAYTOS_` から `ZEIKOS_` へ変えた。** **読む側は、R5 の間だけ旧い名前も読む**
//! ——新しい名前を先に読み、無ければ旧い名前を読んで、そのことを 1 行出す。**子へ渡すときは、旧い名前でも
//! 渡す**（[`env`]）——**`cargo xtask full <コミット>` の子は、そのコミットの木からビルドする**ので、名前を変える前の
//! コミットの子は旧い名前しか読まない。
//!
//! **R5 の最後の項目の後に、このモジュールごと外す**（呼ぶ側は `std::env::var` に戻す）。Python の側
//! （`tools/check_lock.py`）と push の hook（`.claude/hooks/deny_push_when_red.py`）も、同じときに外す。

use std::ffi::OsStr;
use std::process::Command;

/// 新しい名前と旧い名前の組。
const PAIRS: &[(&str, &str)] = &[
    ("ZEIKOS_PUSH_UNCHECKED", "ZAYTOS_PUSH_UNCHECKED"),
    ("ZEIKOS_CHECK_LOCK_OWNER", "ZAYTOS_CHECK_LOCK_OWNER"),
    ("ZEIKOS_CHECK_LOG", "ZAYTOS_CHECK_LOG"),
    ("ZEIKOS_CHECK_DISK_START", "ZAYTOS_CHECK_DISK_START"),
    ("ZEIKOS_CHECK_START_STATE", "ZAYTOS_CHECK_START_STATE"),
    ("ZEIKOS_CHECK_JOBS", "ZAYTOS_CHECK_JOBS"),
];

/// 新しい名前に対応する旧い名前。
pub fn old_name(name: &str) -> Option<&'static str> {
    PAIRS
        .iter()
        .find(|(new, _)| *new == name)
        .map(|(_, old)| *old)
}

/// 読む（純粋な論理）。**新しい名前を先に読み、無ければ旧い名前を読む。** 読んだ名前と値を返す。
fn lookup(name: &str, get: impl Fn(&str) -> Option<String>) -> Option<(&str, String)> {
    if let Some(value) = get(name) {
        return Some((name, value));
    }
    let old = old_name(name)?;
    get(old).map(|value| (old, value))
}

/// 環境変数を読み、読んだ名前も返す。**旧い名前で読んだときは、そのことを標準エラーへ 1 行出す。**
pub fn read(name: &str) -> Option<(&str, String)> {
    let found = lookup(name, |key| std::env::var(key).ok());
    if let Some((used, _)) = &found {
        if *used != name {
            eprintln!(
                "(info) read the old name {used}; the new name is {name} (the old name is read only \
                 until the end of the rename stage R5)"
            );
        }
    }
    found
}

/// 環境変数を読む（[`read`] の値だけ）。
pub fn var(name: &str) -> Option<String> {
    read(name).map(|(_, value)| value)
}

/// 子へ渡す。**旧い名前でも渡す**（モジュールの doc）。
pub fn env(command: &mut Command, name: &str, value: impl AsRef<OsStr>) {
    command.env(name, value.as_ref());
    if let Some(old) = old_name(name) {
        command.env(old, value.as_ref());
    }
}

/// 子へ渡さないように外す。**旧い名前も外す**——親の環境に旧い名前が残っていても、子が読まないように。
pub fn env_remove(command: &mut Command, name: &str) {
    command.env_remove(name);
    if let Some(old) = old_name(name) {
        command.env_remove(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    /// 4 通り（新しい名前だけ・旧い名前だけ・両方・どちらも無い）。**両方なら新しい名前を使う。**
    #[test]
    fn reads_the_new_name_first_and_the_old_name_only_when_the_new_one_is_absent() {
        let new = "ZEIKOS_CHECK_JOBS";
        let old = "ZAYTOS_CHECK_JOBS";
        assert_eq!(
            lookup(new, env_of(&[(new, "2")])),
            Some((new, "2".to_string()))
        );
        assert_eq!(
            lookup(new, env_of(&[(old, "3")])),
            Some((old, "3".to_string()))
        );
        assert_eq!(
            lookup(new, env_of(&[(new, "2"), (old, "3")])),
            Some((new, "2".to_string()))
        );
        assert_eq!(lookup(new, env_of(&[])), None);
    }

    /// 空の値も「在る」と読む。**push の旗の空の理由は、どちらの名前でも呼ぶ側が断る**（`full_check`）。
    #[test]
    fn an_empty_value_is_read_under_either_name() {
        let new = "ZEIKOS_PUSH_UNCHECKED";
        let old = "ZAYTOS_PUSH_UNCHECKED";
        assert_eq!(
            lookup(new, env_of(&[(new, "")])),
            Some((new, String::new()))
        );
        assert_eq!(
            lookup(new, env_of(&[(old, "")])),
            Some((old, String::new()))
        );
    }

    /// 移した 6 つの名前が、どれも表に在ること（綴りの誤りを拾う）。
    #[test]
    fn every_renamed_variable_has_its_old_name() {
        let names = [
            crate::full_check::OVERRIDE_ENV,
            crate::check_lock::OWNER_ENV,
            crate::full_check::LOG_ENV,
            crate::full_check::DISK_START_ENV,
            crate::full_check::START_STATE_ENV,
            crate::CHECK_JOBS_ENV,
        ];
        for name in names {
            assert!(name.starts_with("ZEIKOS_"), "{name}");
            let expected = name.replacen("ZEIKOS_", "ZAYTOS_", 1);
            assert_eq!(old_name(name), Some(expected.as_str()), "{name}");
        }
        assert_eq!(PAIRS.len(), names.len());
    }

    fn envs_of(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    /// 子へは、新しい名前と旧い名前の両方で渡す。**表に無い名前は、その名前だけで渡す。**
    #[test]
    fn env_passes_the_value_under_both_names() {
        let mut command = Command::new("true");
        env(&mut command, "ZEIKOS_CHECK_LOG", "/x.log");
        env(&mut command, "SOME_OTHER", "1");
        let envs = envs_of(&command);
        for (key, value) in [
            ("ZEIKOS_CHECK_LOG", "/x.log"),
            ("ZAYTOS_CHECK_LOG", "/x.log"),
            ("SOME_OTHER", "1"),
        ] {
            assert!(
                envs.contains(&(key.to_string(), Some(value.to_string()))),
                "{key}: {envs:?}"
            );
        }
        assert_eq!(envs.len(), 3, "{envs:?}");
    }

    /// 外すときも、両方の名前を外す。
    #[test]
    fn env_remove_removes_both_names() {
        let mut command = Command::new("true");
        env_remove(&mut command, "ZEIKOS_CHECK_LOCK_OWNER");
        let envs = envs_of(&command);
        assert!(envs.contains(&("ZEIKOS_CHECK_LOCK_OWNER".to_string(), None)));
        assert!(envs.contains(&("ZAYTOS_CHECK_LOCK_OWNER".to_string(), None)));
    }

    /// 表に無い名前は、その名前だけを読む。
    #[test]
    fn a_name_without_an_old_name_reads_only_itself() {
        assert_eq!(old_name("SOME_OTHER"), None);
        assert_eq!(
            lookup("SOME_OTHER", env_of(&[("SOME_OTHER", "1")])),
            Some(("SOME_OTHER", "1".to_string()))
        );
        assert_eq!(lookup("SOME_OTHER", env_of(&[])), None);
    }
}
