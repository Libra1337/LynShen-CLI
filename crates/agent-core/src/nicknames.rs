//! The names subagents go by. A task name (`starcore_ui`) addresses an
//! agent; people call it something warmer, so each agent also gets a name
//! from classical imagery, unique among the session's agents.

/// Two-character names from classical poetry and the seasons.
const NAMES: &[&str] = &[
    "青岚", "听雨", "望舒", "扶摇", "知秋", "星河", "云岫", "南乔", "沐白", "清和", "远山", "拾光",
    "晚枫", "朝露", "小满", "霁月", "松间", "鹿鸣", "竹影", "流萤", "长庚", "启明", "栖梧", "漱石",
    "闻笛", "观澜", "映雪", "临川", "归帆", "白露", "惊蛰", "谷雨", "晴川", "疏桐", "晚晴", "微澜",
    "苍梧", "溪亭", "采薇", "蒹葭", "怀瑾", "若谷", "守拙", "抱朴", "明远", "澄心", "照影", "惊鸿",
];

/// A step through NAMES that visits every name: coprime with its length.
const STRIDE: usize = 7;

/// The `index`-th name a session hands out, starting at `seed`, skipping the
/// names `taken`. Past the list, names repeat with a number (`青岚二`).
pub(crate) fn pick(seed: u64, index: u64, taken: &[&str]) -> String {
    let len = NAMES.len();
    let start = (seed as usize % len + index as usize * STRIDE) % len;
    for round in 0.. {
        for step in 0..len {
            let base = NAMES[(start + step * STRIDE) % len];
            let name = match round {
                0 => base.to_string(),
                n => format!("{base}{}", ORDINALS.get(n).copied().unwrap_or("又")),
            };
            if !taken.contains(&name.as_str()) {
                return name;
            }
        }
    }
    unreachable!()
}

const ORDINALS: &[&str] = &["", "二", "三", "四", "五", "六", "七", "八", "九"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_vary_by_session() {
        let mut taken: Vec<String> = Vec::new();
        for index in 0..NAMES.len() as u64 + 3 {
            let refs: Vec<&str> = taken.iter().map(String::as_str).collect();
            let name = pick(5, index, &refs);
            assert!(!taken.contains(&name), "{name} twice");
            taken.push(name);
        }
        assert!(
            taken[NAMES.len()..]
                .iter()
                .all(|name| name.chars().count() == 3),
            "{taken:?}"
        );
        assert_ne!(pick(1, 0, &[]), pick(2, 0, &[]));
        assert!(!NAMES.len().is_multiple_of(STRIDE));
    }
}
