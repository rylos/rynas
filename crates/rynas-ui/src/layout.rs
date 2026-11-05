//! Dashboard layout model: order, size and visibility of the widgets.
//!
//! Saved in localStorage (`rynas.dashboard`) as a compact string, e.g.
//! `v1 cpu:M system:S !memory:S …` (`!` = hidden). Unknown ids are dropped and widgets missing
//! from the string are appended with their defaults, so adding a widget in a later release never
//! breaks a saved layout. The format is meant to move to the server per user later on.

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum W {
    System,
    Cpu,
    Memory,
    Network,
    Disk,
    Health,
    Volumes,
    Hardware,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Size {
    S,
    M,
    L,
}

impl Size {
    pub fn code(self) -> &'static str {
        match self {
            Size::S => "S",
            Size::M => "M",
            Size::L => "L",
        }
    }
    fn parse(s: &str) -> Option<Size> {
        match s {
            "S" => Some(Size::S),
            "M" => Some(Size::M),
            "L" => Some(Size::L),
            _ => None,
        }
    }
}

pub const ALL: [W; 8] = [W::System, W::Cpu, W::Memory, W::Network, W::Disk, W::Health, W::Volumes, W::Hardware];

impl W {
    pub fn id(self) -> &'static str {
        match self {
            W::System => "system",
            W::Cpu => "cpu",
            W::Memory => "memory",
            W::Network => "network",
            W::Disk => "disk",
            W::Health => "health",
            W::Volumes => "volumes",
            W::Hardware => "hardware",
        }
    }
    fn parse(s: &str) -> Option<W> {
        ALL.into_iter().find(|w| w.id() == s)
    }
    /// i18n key of the title.
    pub fn title(self) -> &'static str {
        match self {
            W::System => "w.system",
            W::Cpu => "w.cpu",
            W::Memory => "w.memory",
            W::Network => "w.network",
            W::Disk => "w.disk_io",
            W::Health => "w.health",
            W::Volumes => "w.volumes",
            W::Hardware => "w.hardware",
        }
    }
    /// Sizes the widget supports, with their width in columns of the 12-column grid.
    pub fn sizes(self) -> &'static [(Size, u8)] {
        match self {
            W::System | W::Memory | W::Health => &[(Size::S, 4), (Size::M, 6)],
            W::Cpu => &[(Size::S, 4), (Size::M, 8), (Size::L, 12)],
            W::Network | W::Disk => &[(Size::S, 4), (Size::M, 6), (Size::L, 8)],
            W::Volumes => &[(Size::S, 4), (Size::M, 8), (Size::L, 12)],
            W::Hardware => &[(Size::M, 6), (Size::L, 12)],
        }
    }
    pub fn span(self, size: Size) -> u8 {
        let s = self.sizes();
        s.iter().find(|(z, _)| *z == size).or(s.first()).map(|(_, c)| *c).unwrap_or(4)
    }
    fn default_size(self) -> Size {
        match self {
            W::Cpu | W::Volumes => Size::M,
            W::Hardware => Size::L,
            _ => Size::S,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Item {
    pub w: W,
    pub size: Size,
    pub hidden: bool,
}

/// Rows of 12: [System 4 | CPU 8] [Memory | Network | Disk I/O] [Health 4 | Volumes 8] [Hardware].
pub fn default_layout() -> Vec<Item> {
    ALL.into_iter().map(|w| Item { w, size: w.default_size(), hidden: false }).collect()
}

pub fn encode(items: &[Item]) -> String {
    let mut s = String::from("v1");
    for i in items {
        s.push(' ');
        if i.hidden {
            s.push('!');
        }
        s.push_str(i.w.id());
        s.push(':');
        s.push_str(i.size.code());
    }
    s
}

pub fn decode(s: &str) -> Vec<Item> {
    let mut parts = s.split_whitespace();
    if parts.next() != Some("v1") {
        return default_layout();
    }
    let mut out: Vec<Item> = Vec::new();
    for p in parts {
        let (hidden, p) = match p.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, p),
        };
        let (id, size) = p.split_once(':').unwrap_or((p, ""));
        let Some(w) = W::parse(id) else { continue };
        if out.iter().any(|i| i.w == w) {
            continue;
        }
        let size = Size::parse(size).filter(|z| w.sizes().iter().any(|(s, _)| s == z)).unwrap_or(w.default_size());
        out.push(Item { w, size, hidden });
    }
    for w in ALL {
        if !out.iter().any(|i| i.w == w) {
            out.push(Item { w, size: w.default_size(), hidden: false });
        }
    }
    out
}

/// Move `w` so that it lands where `target` is now (drag over another widget).
pub fn move_to(items: &mut Vec<Item>, w: W, target: W) -> bool {
    let (Some(from), Some(to)) = (items.iter().position(|i| i.w == w), items.iter().position(|i| i.w == target)) else {
        return false;
    };
    if from == to {
        return false;
    }
    let it = items.remove(from);
    items.insert(to, it);
    true
}

/// Move `w` by `delta` places among the widgets in `visible` (keyboard), skipping hidden ones.
/// Returns the new 0-based position among the visible widgets.
pub fn move_by(items: &mut Vec<Item>, visible: &[W], w: W, delta: isize) -> Option<usize> {
    let pos = visible.iter().position(|v| *v == w)?;
    let new = (pos as isize + delta).clamp(0, visible.len() as isize - 1) as usize;
    if new == pos {
        return None;
    }
    move_to(items, w, visible[new]);
    Some(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut l = default_layout();
        l[2].hidden = true;
        l[1].size = Size::L;
        assert_eq!(decode(&encode(&l)), l);
    }

    #[test]
    fn tolerant_decode() {
        // unknown ids and sizes are ignored, missing widgets are appended, duplicates dropped
        let l = decode("v1 cpu:L bogus:S !memory:XL cpu:S");
        assert_eq!(l[0], Item { w: W::Cpu, size: Size::L, hidden: false });
        assert_eq!(l[1], Item { w: W::Memory, size: Size::S, hidden: true });
        assert_eq!(l.len(), ALL.len());
        assert_eq!(decode("garbage"), default_layout());
        // a size the widget does not support falls back to its default
        assert_eq!(decode("v1 hardware:S")[0].size, Size::L);
    }

    #[test]
    fn default_rows_fill_twelve_columns() {
        let spans: Vec<u8> = default_layout().iter().map(|i| i.w.span(i.size)).collect();
        assert_eq!(spans, [4, 8, 4, 4, 4, 4, 8, 12]);
    }

    #[test]
    fn moves() {
        let mut l = default_layout();
        assert!(move_to(&mut l, W::Hardware, W::System));
        assert_eq!(l[0].w, W::Hardware);
        let visible: Vec<W> = l.iter().map(|i| i.w).collect();
        assert_eq!(move_by(&mut l, &visible, W::Hardware, 1), Some(1));
        assert_eq!(l[1].w, W::Hardware);
        let visible: Vec<W> = l.iter().map(|i| i.w).collect();
        assert_eq!(move_by(&mut l, &visible, W::System, -5), None);
    }
}
