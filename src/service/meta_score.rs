//! 元数据完整度评分（借鉴 booklore 的 `metadataMatchScore`）
//!
//! 用途：在书籍详情里指出「这本书还缺什么元数据」。booklore 拿它在列表页高亮不全的条目；
//! 我们先用于详情页的一行提示——知道缺什么，才知道要不要手工补。
//!
//! 计分方式是**加权完整度**，不是匹配度：每个字段有权重，填了就得分，
//! 最后 `(得分 / 总权重) × 100` 取整。权重按「读者实际在意的程度」排：
//! 书名/作者/封面是书架上一眼就看到的，缺了最难受；出版社/出版日期属于锦上添花。
//!
//! **已锁定的字段视为已填**（booklore 同样如此）：用户手工确认过的内容就是最终值，
//! 不该因为「跟文件里的不一样」而被算作不完整。

use std::collections::HashSet;

/// 参与评分的字段与权重
const WEIGHTS: &[(&str, u32)] = &[
    ("name", 25),
    ("author", 20),
    ("coverUrl", 20),
    ("intro", 15),
    ("kind", 8),
    ("language", 4),
    ("publisher", 4),
    ("publishedAt", 4),
];

/// 评分结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaScore {
    /// 0–100
    pub score: u32,
    /// 缺失字段（按权重从高到低）——前端可直接列出来提示用户补哪几项
    pub missing: Vec<String>,
}

/// 计算一本书的元数据完整度。
///
/// `locked` 为已锁定字段集合（见 `storage::locked_fields`）；`get` 按字段名取当前值。
pub fn score_of<'a, F>(locked: &HashSet<String>, mut get: F) -> MetaScore
where
    F: FnMut(&str) -> Option<&'a str>,
{
    let total: u32 = WEIGHTS.iter().map(|(_, w)| *w).sum();
    let mut got = 0u32;
    let mut missing: Vec<String> = Vec::new();
    for (field, weight) in WEIGHTS {
        let filled = locked.contains(*field)
            || get(field).map(|v| !v.trim().is_empty()).unwrap_or(false);
        if filled {
            got += weight;
        } else {
            missing.push((*field).to_string());
        }
    }
    // WEIGHTS 已按权重降序排列，missing 天然也是降序
    MetaScore {
        score: if total == 0 { 0 } else { got * 100 / total },
        missing,
    }
}

/// 直接对一行书计算（books 表字段映射）
pub fn score_book(book: &crate::model::Book, locked: &HashSet<String>) -> MetaScore {
    score_of(locked, |field| match field {
        "name" => Some(book.name.as_str()),
        "author" => Some(book.author.as_str()),
        // 自定义封面/简介优先——用户设过就算填了
        "coverUrl" => book
            .custom_cover_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(book.cover_url.as_deref()),
        "intro" => book
            .custom_intro
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(book.intro.as_deref()),
        "kind" => book
            .custom_tag
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(book.kind.as_deref()),
        "language" => book.language.as_deref(),
        "publisher" => book.publisher.as_deref(),
        "publishedAt" => book.published_at.as_deref(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn 全填满分_全空零分() {
        let full = score_of(&empty(), |_| Some("有值"));
        assert_eq!(full.score, 100);
        assert!(full.missing.is_empty());

        let none = score_of(&empty(), |_| None);
        assert_eq!(none.score, 0);
        assert_eq!(none.missing.len(), WEIGHTS.len());
    }

    #[test]
    fn 空白字符串不算已填() {
        let s = score_of(&empty(), |f| if f == "name" { Some("   ") } else { None });
        assert_eq!(s.score, 0, "只有空格的书名不该算填了");
        assert!(s.missing.contains(&"name".to_string()));
    }

    #[test]
    fn 已锁定字段视为已填() {
        // 用户手工确认过的值就是最终值，不该因为与文件不一致而算作不完整
        let mut locked = HashSet::new();
        locked.insert("name".to_string());
        locked.insert("author".to_string());
        let s = score_of(&locked, |_| None);
        assert_eq!(s.score, (25 + 20) * 100 / 100);
        assert!(!s.missing.contains(&"name".to_string()));
    }

    #[test]
    fn 缺失项按权重降序() {
        let s = score_of(&empty(), |_| None);
        assert_eq!(
            s.missing.first().map(String::as_str),
            Some("name"),
            "最该补的排最前"
        );
        assert_eq!(s.missing.last().map(String::as_str), Some("publishedAt"));
    }

    #[test]
    fn 权重合计为100便于直接读作百分比() {
        assert_eq!(WEIGHTS.iter().map(|(_, w)| *w).sum::<u32>(), 100);
    }

    #[test]
    fn 自定义字段优先() {
        let mut b = crate::model::Book {
            name: "书".into(),
            author: "作者".into(),
            ..Default::default()
        };
        let base = score_book(&b, &empty()).score;
        b.custom_intro = Some("我写的简介".into());
        let with_custom = score_book(&b, &empty()).score;
        assert!(
            with_custom > base,
            "用户自己写的简介应计入完整度（{base} → {with_custom}）"
        );
    }
}
